//! `org.lunaris.InputManager1` D-Bus service.
//!
//! Apps register keybindings here dynamically. Static bindings continue
//! to live in `~/.config/lunaris/compositor.toml`; dynamic bindings are
//! ephemeral and disappear automatically when the registering client
//! disconnects (cleaned up via `NameOwnerChanged`).
//!
//! Two scopes:
//! * `app_focused` — only fires when the focused toplevel's `app_id`
//!   matches the registration's `app_id`. Intended for per-app
//!   shortcuts (Ctrl+S in an editor).
//! * `app_global` — fires regardless of focus. Reserved for first-party
//!   apps; third-party apps without the `input.register_global_bindings`
//!   permission should be rejected at install time. This service does
//!   *not* enforce that policy — it only accepts the string — so a
//!   future hook into the permission system is required.
//!
//! On dispatch, the compositor invokes [`InputManagerState::emit_binding_invoked`]
//! which emits the `BindingInvoked` D-Bus signal to the owning client
//! only (via `SignalEmitter::set_destination`).

use futures_executor::ThreadPool;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use zbus::{
    message::Header,
    names::UniqueName,
    object_server::SignalEmitter,
    zvariant::Type,
};

use super::app_interface::AppRegistry;
use super::name_owners::NameOwners;

const OBJECT_PATH: &str = "/org/lunaris/InputManager1";
const SERVICE_NAME: &str = "org.lunaris.InputManager1";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Single registered binding, as returned by `QueryBindings`.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct BindingInfo {
    /// Accelerator, e.g. `"Ctrl+S"`.
    pub binding: String,
    /// Opaque action id chosen by the client. Echoed back when the
    /// binding fires.
    pub action: String,
    /// `"app_focused"` or `"app_global"`.
    pub scope: String,
    /// D-Bus unique name of the owning client, e.g. `":1.42"`.
    pub owner: String,
    /// App id the registration is scoped to. Required for `app_focused`,
    /// empty string for `app_global`.
    pub app_id: String,
}

/// Conflict description returned from `RegisterBinding` on failure and
/// from `QueryConflicts`.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct ConflictInfo {
    pub binding: String,
    pub existing_action: String,
    pub existing_scope: String,
    pub existing_owner: String,
}

/// Result struct for `RegisterBinding`. A boolean alone would be
/// insufficient because the caller wants to know *which* existing
/// binding is in the way when the call fails. Represented as a tuple
/// `(success, conflict_binding, conflict_action, conflict_scope, conflict_owner)`
/// because zbus `Type` derive on `Option<ConflictInfo>` is awkward.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct RegisterResult {
    pub success: bool,
    pub conflict: Vec<ConflictInfo>,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Inner shared store of dynamic bindings.
#[derive(Debug, Default)]
pub struct DynamicBindings {
    /// Map from D-Bus unique name to that client's registered bindings.
    /// Stored by owner so `UnregisterAll` is O(1).
    pub by_owner: HashMap<String, Vec<BindingInfo>>,
}

impl DynamicBindings {
    /// Flatten into a single vec, preserving order of insertion per owner.
    pub fn all(&self) -> Vec<BindingInfo> {
        self.by_owner.values().flatten().cloned().collect()
    }

    /// Find all entries whose accelerator equals `binding`.
    pub fn find_conflicts(&self, binding: &str) -> Vec<BindingInfo> {
        self.by_owner
            .values()
            .flatten()
            .filter(|b| b.binding == binding)
            .cloned()
            .collect()
    }

    /// Insert, returning true if a new entry was added.
    pub fn insert(&mut self, info: BindingInfo) {
        self.by_owner
            .entry(info.owner.clone())
            .or_default()
            .push(info);
    }

    /// Remove a single binding by owner + accelerator. Returns true if
    /// something was removed.
    pub fn remove(&mut self, owner: &str, binding: &str) -> bool {
        let Some(entries) = self.by_owner.get_mut(owner) else {
            return false;
        };
        let before = entries.len();
        entries.retain(|b| b.binding != binding);
        if entries.is_empty() {
            self.by_owner.remove(owner);
        }
        before != self.by_owner.get(owner).map(|v| v.len()).unwrap_or(0)
    }

    /// Drop all bindings for an owner. Returns the number removed.
    pub fn remove_owner(&mut self, owner: &str) -> usize {
        self.by_owner.remove(owner).map(|v| v.len()).unwrap_or(0)
    }
}

/// State held on `Common` so the input dispatcher can look up dynamic
/// bindings and emit signals back to the registering client.
#[derive(Debug, Clone)]
pub struct InputManagerState {
    /// Shared binding store, also held by the D-Bus interface impl.
    pub bindings: Arc<Mutex<DynamicBindings>>,
    /// Shared app registration store. Used by `register_binding` to
    /// verify that a claimed `app_id` matches the caller's
    /// `org.lunaris.App1` registration.
    pub app_registry: Arc<Mutex<AppRegistry>>,
    /// Background executor used to drive async signal emissions from
    /// the (synchronous) input path.
    executor: ThreadPool,
    /// Populated once the D-Bus service finishes starting. Before that
    /// point, bindings can be registered at the interface level but
    /// signals cannot be emitted.
    conn: Arc<OnceLock<zbus::Connection>>,
}

impl InputManagerState {
    /// Create state and kick off the async D-Bus service startup.
    ///
    /// The service starts in the background on `executor`; it is
    /// normal for [`Self::is_ready`] to return `false` for a brief
    /// window after construction. Registrations arriving before the
    /// bus is up are dropped at the D-Bus layer (no client can find
    /// the service yet) so there is no race on our side.
    pub fn new(executor: &ThreadPool, app_registry: Arc<Mutex<AppRegistry>>) -> Self {
        let bindings = Arc::new(Mutex::new(DynamicBindings::default()));
        let conn_cell: Arc<OnceLock<zbus::Connection>> = Arc::new(OnceLock::new());

        let bindings_for_serve = bindings.clone();
        let app_registry_for_serve = app_registry.clone();
        let conn_for_serve = conn_cell.clone();
        let executor_for_serve = executor.clone();
        executor.spawn_ok(async move {
            match serve(
                bindings_for_serve,
                app_registry_for_serve,
                &executor_for_serve,
            )
            .await
            {
                Ok(conn) => {
                    let _ = conn_for_serve.set(conn);
                    tracing::info!("input_manager: D-Bus service started on {SERVICE_NAME}");
                }
                Err(err) => {
                    tracing::error!("input_manager: failed to serve {SERVICE_NAME}: {err}");
                }
            }
        });

        Self {
            bindings,
            app_registry,
            executor: executor.clone(),
            conn: conn_cell,
        }
    }

    /// True once the D-Bus connection is up.
    pub fn is_ready(&self) -> bool {
        self.conn.get().is_some()
    }

    /// Emit the `BindingInvoked` signal to the client that owns
    /// `binding`. No-op if the binding is unknown or the bus is not
    /// ready yet — both cases are logged at debug level to keep the
    /// hot input path noise-free in production.
    pub fn emit_binding_invoked(&self, binding: &str, action: &str, owner: &str) {
        let Some(conn) = self.conn.get().cloned() else {
            tracing::debug!("input_manager: bus not ready, dropping signal for {binding}");
            return;
        };
        let Ok(owner_name) = UniqueName::try_from(owner.to_owned()) else {
            tracing::warn!("input_manager: invalid owner name {owner}, dropping signal");
            return;
        };
        let binding = binding.to_string();
        let action = action.to_string();
        self.executor.spawn_ok(async move {
            let Ok(emitter) = SignalEmitter::new(&conn, OBJECT_PATH) else {
                tracing::warn!("input_manager: could not build SignalEmitter");
                return;
            };
            let emitter = emitter.set_destination(owner_name.into());
            if let Err(err) = InputManager::binding_invoked(&emitter, binding, action).await {
                tracing::warn!("input_manager: emit_binding_invoked failed: {err}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// D-Bus interface
// ---------------------------------------------------------------------------

struct InputManager {
    bindings: Arc<Mutex<DynamicBindings>>,
    app_registry: Arc<Mutex<AppRegistry>>,
    name_owners: NameOwners,
    /// Background executor used by the cleanup task to run the
    /// NameOwnerChanged loop.
    executor: ThreadPool,
}

impl InputManager {
    /// Spawn a background task that removes all bindings belonging to
    /// clients that disconnect from the bus. This is what makes the
    /// service safe against crashing clients.
    fn start_cleanup_task(&self, conn: &zbus::Connection) {
        let bindings = self.bindings.clone();
        let conn = conn.clone();
        self.executor.spawn_ok(async move {
            let proxy = match zbus::fdo::DBusProxy::new(&conn).await {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!(
                        "input_manager: cleanup: cannot bind DBusProxy: {err}"
                    );
                    return;
                }
            };
            let mut stream = match proxy.receive_name_owner_changed().await {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(
                        "input_manager: cleanup: receive_name_owner_changed failed: {err}"
                    );
                    return;
                }
            };
            use futures_util::StreamExt;
            while let Some(signal) = stream.next().await {
                let Ok(args) = signal.args() else { continue };
                // An empty new_owner means the name was released and
                // has no replacement — the typical "client exited" case.
                if !args.new_owner.is_none() {
                    continue;
                }
                let name = args.name.to_string();
                let removed = {
                    let mut store = bindings.lock().unwrap();
                    store.remove_owner(&name)
                };
                if removed > 0 {
                    tracing::info!(
                        "input_manager: cleaned up {} binding(s) for exited client {}",
                        removed,
                        name
                    );
                }
            }
        });
    }
}

fn validate_scope(scope: &str) -> zbus::fdo::Result<()> {
    match scope {
        "app_focused" | "app_global" => Ok(()),
        other => Err(zbus::fdo::Error::InvalidArgs(format!(
            "scope must be app_focused or app_global, got {other:?}"
        ))),
    }
}

fn validate_binding(binding: &str) -> zbus::fdo::Result<()> {
    if binding.is_empty() {
        return Err(zbus::fdo::Error::InvalidArgs(
            "binding must not be empty".into(),
        ));
    }
    match crate::config::parse_keybinding(binding) {
        Some(_) => Ok(()),
        None => Err(zbus::fdo::Error::InvalidArgs(format!(
            "binding {binding:?} is not a valid accelerator"
        ))),
    }
}

/// Policy for register-time conflict detection.
///
/// Two dynamic entries with the same accelerator conflict if:
/// * either side is `app_global` (global binds everywhere, so any
///   other registration on the same accelerator would be shadowed),
/// * or both are `app_focused` AND share the same `app_id` (two apps
///   can each have `Ctrl+S` at app-focused scope because only one
///   window is focused at a time).
fn registration_dominates(existing: &BindingInfo, new_scope: &str, new_app_id: &str) -> bool {
    if existing.scope == "app_global" || new_scope == "app_global" {
        return true;
    }
    existing.app_id == new_app_id
}

#[zbus::interface(name = "org.lunaris.InputManager1")]
impl InputManager {
    /// Register a keybinding for the calling client.
    ///
    /// `scope` is `"app_focused"` (fires only when a window belonging
    /// to `app_id` is focused) or `"app_global"` (fires regardless of
    /// focus; reserved for first-party apps).
    ///
    /// Returns `success = true` on success, or `success = false` plus
    /// one or more entries in `conflict` describing the dominating
    /// existing bindings.
    async fn register_binding(
        &self,
        #[zbus(header)] header: Header<'_>,
        binding: String,
        action: String,
        scope: String,
        app_id: String,
    ) -> zbus::fdo::Result<RegisterResult> {
        validate_scope(&scope)?;
        validate_binding(&binding)?;

        let Some(sender) = header.sender() else {
            return Err(zbus::fdo::Error::Failed(
                "no sender on D-Bus message".into(),
            ));
        };
        let owner = sender.to_string();

        // Scope-specific invariants.
        if scope == "app_focused" && app_id.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "app_focused scope requires a non-empty app_id".into(),
            ));
        }

        // For app_focused, the claimed `app_id` must match the one
        // the caller registered via `org.lunaris.App1::RegisterApp`.
        // Without this check any client could claim any app_id and
        // thereby intercept shortcuts meant for another app as soon
        // as that app gets focus.
        if scope == "app_focused" {
            let reg = self.app_registry.lock().unwrap();
            match reg.by_owner(&owner) {
                Some(info) if info.app_id == app_id => {}
                Some(info) => {
                    return Err(zbus::fdo::Error::AccessDenied(format!(
                        "app_id mismatch: caller registered as {:?} but claimed {:?}",
                        info.app_id, app_id
                    )));
                }
                None => {
                    return Err(zbus::fdo::Error::AccessDenied(
                        "app_focused scope requires RegisterApp on org.lunaris.App1 first"
                            .into(),
                    ));
                }
            }
        }

        // For app_global, the caller must have declared the
        // `register_global_bindings` permission at RegisterApp time.
        // Unknown clients (no prior RegisterApp) are rejected; known
        // clients without the permission are rejected with a message
        // pointing at the permission they need.
        if scope == "app_global" {
            let reg = self.app_registry.lock().unwrap();
            match reg.by_owner(&owner) {
                Some(info) if info.can_register_global_bindings() => {}
                Some(info) => {
                    return Err(zbus::fdo::Error::AccessDenied(format!(
                        "app_global scope requires the 'register_global_bindings' \
                         permission; {:?} registered without it",
                        info.app_id
                    )));
                }
                None => {
                    return Err(zbus::fdo::Error::AccessDenied(
                        "app_global scope requires RegisterApp with \
                         'register_global_bindings' permission first"
                            .into(),
                    ));
                }
            }
        }

        let mut store = self.bindings.lock().unwrap();

        // Conflict detection: reject if any existing registration with
        // the same accelerator would dominate the new one.
        let existing = store.find_conflicts(&binding);
        let mut conflicts = Vec::new();
        for ex in &existing {
            if registration_dominates(ex, &scope, &app_id) {
                conflicts.push(ConflictInfo {
                    binding: binding.clone(),
                    existing_action: ex.action.clone(),
                    existing_scope: ex.scope.clone(),
                    existing_owner: ex.owner.clone(),
                });
            }
        }
        if !conflicts.is_empty() {
            return Ok(RegisterResult {
                success: false,
                conflict: conflicts,
            });
        }

        let info = BindingInfo {
            binding: binding.clone(),
            action,
            scope,
            owner: owner.clone(),
            app_id,
        };
        store.insert(info);
        tracing::debug!("input_manager: registered {binding} for {owner}");
        Ok(RegisterResult {
            success: true,
            conflict: Vec::new(),
        })
    }

    /// Remove a single binding previously registered by the calling
    /// client. Returns true if a binding was actually removed.
    async fn unregister_binding(
        &self,
        #[zbus(header)] header: Header<'_>,
        binding: String,
    ) -> zbus::fdo::Result<bool> {
        let Some(sender) = header.sender() else {
            return Err(zbus::fdo::Error::Failed(
                "no sender on D-Bus message".into(),
            ));
        };
        let mut store = self.bindings.lock().unwrap();
        Ok(store.remove(&sender.to_string(), &binding))
    }

    /// Remove every binding registered by the calling client. Returns
    /// the number of bindings that were removed.
    async fn unregister_all(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<u32> {
        let Some(sender) = header.sender() else {
            return Err(zbus::fdo::Error::Failed(
                "no sender on D-Bus message".into(),
            ));
        };
        let mut store = self.bindings.lock().unwrap();
        Ok(store.remove_owner(&sender.to_string()) as u32)
    }

    /// List registered bindings. Pass empty string for `scope_filter`
    /// to return everything.
    async fn query_bindings(&self, scope_filter: String) -> zbus::fdo::Result<Vec<BindingInfo>> {
        let store = self.bindings.lock().unwrap();
        let all = store.all();
        if scope_filter.is_empty() {
            Ok(all)
        } else {
            Ok(all.into_iter().filter(|b| b.scope == scope_filter).collect())
        }
    }

    /// List every existing binding that uses the given accelerator.
    /// Used by Settings to show a live conflict indicator while the
    /// user is typing a new shortcut.
    async fn query_conflicts(&self, binding: String) -> zbus::fdo::Result<Vec<ConflictInfo>> {
        let store = self.bindings.lock().unwrap();
        Ok(store
            .find_conflicts(&binding)
            .into_iter()
            .map(|b| ConflictInfo {
                binding: b.binding,
                existing_action: b.action,
                existing_scope: b.scope,
                existing_owner: b.owner,
            })
            .collect())
    }

    /// Emitted when a registered keystroke fires. Delivered only to
    /// the owning client via `SignalEmitter::set_destination`.
    #[zbus(signal)]
    async fn binding_invoked(
        ctx: &SignalEmitter<'_>,
        binding: String,
        action: String,
    ) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Service startup
// ---------------------------------------------------------------------------

async fn serve(
    bindings: Arc<Mutex<DynamicBindings>>,
    app_registry: Arc<Mutex<AppRegistry>>,
    executor: &ThreadPool,
) -> zbus::Result<zbus::Connection> {
    let conn = zbus::Connection::session().await?;
    let name_owners = NameOwners::new(&conn, executor).await?;

    let iface = InputManager {
        bindings,
        app_registry,
        name_owners,
        executor: executor.clone(),
    };
    // Kick off cleanup before publishing the object so we never miss a
    // NameOwnerChanged signal for clients that race-register-and-exit.
    iface.start_cleanup_task(&conn);

    conn.object_server().at(OBJECT_PATH, iface).await?;
    conn.request_name(SERVICE_NAME).await?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(binding: &str, scope: &str, owner: &str, app_id: &str) -> BindingInfo {
        BindingInfo {
            binding: binding.into(),
            action: format!("action_for_{binding}"),
            scope: scope.into(),
            owner: owner.into(),
            app_id: app_id.into(),
        }
    }

    #[test]
    fn dynamic_bindings_insert_and_find() {
        let mut store = DynamicBindings::default();
        store.insert(mk("Ctrl+S", "app_focused", ":1.1", "org.editor"));
        store.insert(mk("Ctrl+P", "app_focused", ":1.1", "org.editor"));
        store.insert(mk("Ctrl+S", "app_focused", ":1.2", "org.terminal"));
        assert_eq!(store.all().len(), 3);
        let c = store.find_conflicts("Ctrl+S");
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn remove_owner_cleans_all() {
        let mut store = DynamicBindings::default();
        store.insert(mk("Ctrl+A", "app_global", ":1.10", ""));
        store.insert(mk("Ctrl+B", "app_global", ":1.10", ""));
        store.insert(mk("Ctrl+C", "app_focused", ":1.11", "org.x"));
        assert_eq!(store.remove_owner(":1.10"), 2);
        assert_eq!(store.all().len(), 1);
        assert!(!store.by_owner.contains_key(":1.10"));
    }

    #[test]
    fn remove_single_binding() {
        let mut store = DynamicBindings::default();
        store.insert(mk("Ctrl+A", "app_focused", ":1.1", "org.x"));
        store.insert(mk("Ctrl+B", "app_focused", ":1.1", "org.x"));
        assert!(store.remove(":1.1", "Ctrl+A"));
        let remaining = store.all();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].binding, "Ctrl+B");
    }

    #[test]
    fn remove_last_binding_drops_owner_entry() {
        let mut store = DynamicBindings::default();
        store.insert(mk("Ctrl+A", "app_focused", ":1.1", "org.x"));
        assert!(store.remove(":1.1", "Ctrl+A"));
        assert!(!store.by_owner.contains_key(":1.1"));
    }

    #[test]
    fn registration_dominates_app_global_always_wins() {
        let existing = mk("Ctrl+X", "app_global", ":1.1", "");
        assert!(registration_dominates(&existing, "app_focused", "anything"));
        assert!(registration_dominates(&existing, "app_global", ""));
    }

    #[test]
    fn registration_dominates_same_app_id() {
        let existing = mk("Ctrl+Y", "app_focused", ":1.1", "org.editor");
        assert!(registration_dominates(&existing, "app_focused", "org.editor"));
        assert!(!registration_dominates(
            &existing, "app_focused", "org.other"
        ));
    }

    #[test]
    fn registration_dominates_new_is_global() {
        let existing = mk("Ctrl+Z", "app_focused", ":1.1", "org.editor");
        assert!(registration_dominates(&existing, "app_global", ""));
    }

    #[test]
    fn validate_scope_rejects_unknown() {
        assert!(validate_scope("app_focused").is_ok());
        assert!(validate_scope("app_global").is_ok());
        assert!(validate_scope("global").is_err());
        assert!(validate_scope("").is_err());
    }

    #[test]
    fn validate_binding_rejects_empty_and_garbage() {
        assert!(validate_binding("").is_err());
        // parse_keybinding accepts a single key with no modifiers; for
        // actual shortcut use that is typically a mistake, but at this
        // layer we just check that the grammar parses.
        assert!(validate_binding("Ctrl+Shift+H").is_ok());
    }
}
