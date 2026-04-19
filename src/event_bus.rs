// SPDX-License-Identifier: GPL-3.0-only

//! Non-blocking Event Bus integration for the Lunaris compositor.

mod proto {
    include!(concat!(env!("OUT_DIR"), "/lunaris.eventbus.rs"));
}

use prost::Message as _;
use proto::Event;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};

const DEFAULT_PRODUCER_SOCKET: &str = "/run/lunaris/event-bus-producer.sock";
const CHANNEL_CAPACITY: usize = 4096;

pub struct EventBusMessage(pub Vec<u8>);

#[derive(Clone, Debug)]
pub struct EventBusHandle {
    sender: SyncSender<EventBusMessage>,
    session_id: String,
    /// Last app_id emitted for window.focused, used to deduplicate.
    last_focused_app_id: std::sync::Arc<std::sync::Mutex<String>>,
}

impl EventBusHandle {
    pub fn try_send(&self, msg: EventBusMessage) {
        match self.sender.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                warn!("event bus channel full, dropping message");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    /// Emit a `window.focused` event with the given app ID.
    ///
    /// Deduplicated: only emits if the app_id changed since the last call.
    pub fn emit_window_focused(&self, app_id: &str) {
        {
            let mut last = self.last_focused_app_id.lock().unwrap();
            if *last == app_id {
                return;
            }
            *last = app_id.to_string();
        }
        let event = Event {
            id: uuid::Uuid::now_v7().to_string(),
            r#type: "window.focused".to_string(),
            timestamp: timestamp_micros(),
            source: "wayland".to_string(),
            pid: std::process::id(),
            session_id: self.session_id.clone(),
            payload: vec![],
        };
        if let Some(msg) = encode(event) {
            self.try_send(msg);
            debug!(app_id, "emitted window.focused event");
        }
    }

    /// Emit a `window.opened` event with the given app ID.
    pub fn emit_window_opened(&self, app_id: &str) {
        let event = Event {
            id: uuid::Uuid::now_v7().to_string(),
            r#type: "window.opened".to_string(),
            timestamp: timestamp_micros(),
            source: "wayland".to_string(),
            pid: std::process::id(),
            session_id: self.session_id.clone(),
            payload: vec![],
        };
        if let Some(msg) = encode(event) {
            self.try_send(msg);
            debug!(app_id, "emitted window.opened event");
        }
    }

    /// Emit a `clipboard.copy` event with the given MIME type.
    ///
    /// The clipboard content is never included, only the MIME type.
    pub fn emit_clipboard_copy(&self, mime_type: &str) {
        let event = Event {
            id: uuid::Uuid::now_v7().to_string(),
            r#type: "clipboard.copy".to_string(),
            timestamp: timestamp_micros(),
            source: "wayland".to_string(),
            pid: std::process::id(),
            session_id: self.session_id.clone(),
            payload: vec![],
        };
        if let Some(msg) = encode(event) {
            self.try_send(msg);
            tracing::debug!(mime_type, "emitted clipboard.copy event");
        }
    }

    /// Emit a `clipboard.copy` event with the given MIME type.
    ///
    /// The clipboard content is never included, only the MIME type.
    
    pub fn emit_window_closed(&self, app_id: &str) {
        let event = Event {
            id: uuid::Uuid::now_v7().to_string(),
            r#type: "window.closed".to_string(),
            timestamp: timestamp_micros(),
            source: "wayland".to_string(),
            pid: std::process::id(),
            session_id: self.session_id.clone(),
            payload: vec![],
        };
        if let Some(msg) = encode(event) {
            self.try_send(msg);
            debug!(app_id, "emitted window.closed event");
        }
    }

    /// Emit a `module.action_invoked` event when a keybinding bound via
    /// a module manifest fires. The payload is empty for now — the type
    /// string encodes the module id as `module.action_invoked.<id>` so
    /// subscribers can filter by prefix, and the action id lives in the
    /// `source` field to avoid allocating a new protobuf message for
    /// this single case.
    pub fn emit_module_action(&self, module_id: &str, action_id: &str) {
        let event = Event {
            id: uuid::Uuid::now_v7().to_string(),
            r#type: format!("module.action_invoked.{module_id}"),
            timestamp: timestamp_micros(),
            source: action_id.to_string(),
            pid: std::process::id(),
            session_id: self.session_id.clone(),
            payload: vec![],
        };
        if let Some(msg) = encode(event) {
            self.try_send(msg);
            debug!(
                module_id,
                action_id, "emitted module.action_invoked event"
            );
        }
    }
}

pub fn spawn() -> EventBusHandle {
    let socket_path = std::env::var("LUNARIS_PRODUCER_SOCKET")
        .unwrap_or_else(|_| DEFAULT_PRODUCER_SOCKET.to_string());
    let session_id = std::env::var("LUNARIS_SESSION_ID")
        .unwrap_or_else(|_| uuid::Uuid::now_v7().to_string());
    let (tx, rx) = mpsc::sync_channel::<EventBusMessage>(CHANNEL_CAPACITY);
    thread::Builder::new()
        .name("event-bus-sender".to_string())
        .spawn(move || sender_thread(&socket_path, rx))
        .expect("failed to spawn event-bus sender thread");
    EventBusHandle {
        sender: tx,
        session_id,
        last_focused_app_id: std::sync::Arc::new(std::sync::Mutex::new(String::new())),
    }
}

fn sender_thread(socket_path: &str, rx: mpsc::Receiver<EventBusMessage>) {
    loop {
        let mut stream = loop {
            match UnixStream::connect(socket_path) {
                Ok(s) => {
                    debug!(socket = socket_path, "connected to event bus");
                    break s;
                }
                Err(_) => { thread::sleep(Duration::from_secs(2)); }
            }
        };
        loop {
            match rx.recv() {
                Ok(EventBusMessage(bytes)) => {
                    if stream.write_all(&bytes).is_err() {
                        warn!("event bus connection lost, reconnecting");
                        break;
                    }
                }
                Err(_) => {
                    debug!("event bus sender thread exiting");
                    return;
                }
            }
        }
    }
}

fn encode(event: Event) -> Option<EventBusMessage> {
    let protobuf_bytes = event.encode_to_vec();
    let len = u32::try_from(protobuf_bytes.len()).ok()?;
    let mut out = Vec::with_capacity(4 + protobuf_bytes.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&protobuf_bytes);
    Some(EventBusMessage(out))
}

fn timestamp_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}
