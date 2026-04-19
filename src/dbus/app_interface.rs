//! `org.lunaris.App1` D-Bus service.
//!
//! Lunaris-aware apps call `RegisterApp` once at startup to declare
//! their Wayland `app_id`, a display name, and the set of actions they
//! expose for rebinding. The registration is keyed by the client's
//! D-Bus unique name, so the compositor can correlate:
//!
//! * a `RegisterBinding` call on `org.lunaris.InputManager1` — the
//!   input manager looks up the caller's `app_id` here to enforce the
//!   `app_focused` scope;
//! * a focus change on a Wayland toplevel — the shell looks up whether
//!   the focused toplevel's `app_id` belongs to a registered client and
//!   uses that for `app_focused` dispatch.
//!
//! Registrations are ephemeral: when the client's name disappears from
//! the bus (crash, normal exit), the cleanup task drops both the
//! registration and any keybindings the client had installed.

use futures_executor::ThreadPool;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use zbus::{message::Header, zvariant::Type};

const OBJECT_PATH: &str = "/org/lunaris/App";
const SERVICE_NAME: &str = "org.lunaris.App1";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One registerable action the app exposes to Settings. Apps list
/// every action they can receive at registration time so the Settings
/// UI can offer them for rebinding without the app needing to be
/// running when the user edits its shortcuts.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DeclaredAction {
    pub id: String,
    pub label: String,
    /// Longer explanation shown in Settings tooltips. Empty string
    /// means "no description" — avoids pulling in zbus `Maybe` just
    /// for this optional field.
    pub description: String,
}

/// Runtime information about a currently-registered app.
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    /// D-Bus unique name of the registering client (`":1.42"`).
    pub owner: String,
    pub declared_actions: Vec<DeclaredAction>,
    /// Input-subsystem permissions the client declared at registration.
    /// Known strings: `"register_focused_bindings"`,
    /// `"register_global_bindings"`. Unknown strings are preserved so
    /// forward-compat clients can declare future permissions without
    /// the compositor rejecting them; the compositor only acts on the
    /// strings it recognises.
    pub permissions: Vec<String>,
}

impl AppInfo {
    /// True iff the app declared `"register_global_bindings"` at
    /// `RegisterApp` time. The InputManager gates `app_global`-scope
    /// binding registrations on this.
    pub fn can_register_global_bindings(&self) -> bool {
        self.permissions
            .iter()
            .any(|p| p == "register_global_bindings")
    }
}

// ---------------------------------------------------------------------------
// Shared store
// ---------------------------------------------------------------------------

/// Thread-safe index of registered apps. Shared with [`crate::dbus::input_manager::InputManager`]
/// so `RegisterBinding` can verify that the caller's claimed `app_id`
/// matches the one they declared here.
#[derive(Debug, Default)]
pub struct AppRegistry {
    /// D-Bus unique name → AppInfo. Keyed this way so
    /// `NameOwnerChanged` cleanup is O(1).
    by_owner: HashMap<String, AppInfo>,
}

impl AppRegistry {
    /// Insert (or replace) a registration.
    pub fn insert(&mut self, info: AppInfo) {
        self.by_owner.insert(info.owner.clone(), info);
    }

    /// Look up an app by its D-Bus unique name. Primarily used by
    /// `InputManager::register_binding` to verify ownership.
    pub fn by_owner(&self, owner: &str) -> Option<&AppInfo> {
        self.by_owner.get(owner)
    }

    /// Find the first registration advertising the given Wayland
    /// `app_id`. Used by the shell focus code to decide whether a
    /// newly-focused toplevel has a D-Bus-registered partner.
    pub fn by_app_id(&self, app_id: &str) -> Option<&AppInfo> {
        self.by_owner.values().find(|a| a.app_id == app_id)
    }

    /// Drop the registration for a D-Bus owner. Returns the removed
    /// entry if any, so the caller can log it.
    pub fn remove_owner(&mut self, owner: &str) -> Option<AppInfo> {
        self.by_owner.remove(owner)
    }

    /// Snapshot for introspection (Settings, logs, tests).
    pub fn all(&self) -> Vec<AppInfo> {
        self.by_owner.values().cloned().collect()
    }

    /// True iff at least one app is currently registered. Useful for
    /// the dispatcher fast-path — if no apps are registered the
    /// resolver can skip its `app_focused` branch entirely.
    pub fn is_empty(&self) -> bool {
        self.by_owner.is_empty()
    }
}

// ---------------------------------------------------------------------------
// State + service startup
// ---------------------------------------------------------------------------

/// State held on `Common` that wraps the shared [`AppRegistry`]
/// together with the async D-Bus plumbing.
#[derive(Debug, Clone)]
pub struct AppRegistryState {
    /// Shared registry. Cheaply clonable via the `Arc`; callers that
    /// only need reads can `.lock().unwrap()` and hold the guard for
    /// the shortest time possible.
    pub registry: Arc<Mutex<AppRegistry>>,
    _conn: Arc<OnceLock<zbus::Connection>>,
}

impl AppRegistryState {
    /// Build a new state and start serving the D-Bus interface in the
    /// background. The same pattern as [`InputManagerState::new`]:
    /// construction is synchronous, the `OnceLock` fills in once the
    /// bus is up.
    pub fn new(executor: &ThreadPool) -> Self {
        let registry = Arc::new(Mutex::new(AppRegistry::default()));
        let conn_cell: Arc<OnceLock<zbus::Connection>> = Arc::new(OnceLock::new());

        let registry_for_serve = registry.clone();
        let conn_for_serve = conn_cell.clone();
        let executor_for_serve = executor.clone();
        executor.spawn_ok(async move {
            match serve(registry_for_serve, &executor_for_serve).await {
                Ok(conn) => {
                    let _ = conn_for_serve.set(conn);
                    tracing::info!("app_interface: D-Bus service started on {SERVICE_NAME}");
                }
                Err(err) => {
                    tracing::error!("app_interface: failed to serve {SERVICE_NAME}: {err}");
                }
            }
        });

        Self {
            registry,
            _conn: conn_cell,
        }
    }
}

// ---------------------------------------------------------------------------
// D-Bus interface
// ---------------------------------------------------------------------------

struct AppInterface {
    registry: Arc<Mutex<AppRegistry>>,
    executor: ThreadPool,
}

impl AppInterface {
    /// Background task that removes registrations for clients that
    /// disconnect from the bus. Distinct from the InputManager's own
    /// cleanup watcher so the two services can boot and crash
    /// independently — two cheap `NameOwnerChanged` streams beat one
    /// shared one that couples lifecycles.
    fn start_cleanup_task(&self, conn: &zbus::Connection) {
        let registry = self.registry.clone();
        let conn = conn.clone();
        self.executor.spawn_ok(async move {
            let proxy = match zbus::fdo::DBusProxy::new(&conn).await {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!("app_interface: cleanup: DBusProxy bind failed: {err}");
                    return;
                }
            };
            let mut stream = match proxy.receive_name_owner_changed().await {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(
                        "app_interface: cleanup: receive_name_owner_changed failed: {err}"
                    );
                    return;
                }
            };
            use futures_util::StreamExt;
            while let Some(signal) = stream.next().await {
                let Ok(args) = signal.args() else { continue };
                if !args.new_owner.is_none() {
                    continue;
                }
                let name = args.name.to_string();
                let removed = {
                    let mut reg = registry.lock().unwrap();
                    reg.remove_owner(&name)
                };
                if let Some(info) = removed {
                    tracing::info!(
                        "app_interface: cleaned up registration for exited client {} ({})",
                        info.app_id,
                        name
                    );
                }
            }
        });
    }
}

#[zbus::interface(name = "org.lunaris.App1")]
impl AppInterface {
    /// Register the calling client. `app_id` must match the Wayland
    /// `app_id` the client advertises on its toplevels — the
    /// InputManager and shell rely on that identity to route
    /// focus-scoped keybindings.
    ///
    /// Calling this a second time overwrites the previous entry for
    /// the same D-Bus owner. That lets an app update its declared
    /// action list without having to crash-cycle its bus connection.
    async fn register_app(
        &self,
        #[zbus(header)] header: Header<'_>,
        app_id: String,
        name: String,
        actions: Vec<DeclaredAction>,
        permissions: Vec<String>,
    ) -> zbus::fdo::Result<bool> {
        if app_id.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "app_id must not be empty".into(),
            ));
        }
        let Some(sender) = header.sender() else {
            return Err(zbus::fdo::Error::Failed(
                "no sender on D-Bus message".into(),
            ));
        };
        let owner = sender.to_string();
        let info = AppInfo {
            app_id: app_id.clone(),
            name,
            owner: owner.clone(),
            declared_actions: actions,
            permissions,
        };
        self.registry.lock().unwrap().insert(info);
        tracing::debug!("app_interface: registered {app_id} for {owner}");
        Ok(true)
    }

    /// Explicitly unregister. Optional — crash cleanup via
    /// `NameOwnerChanged` handles the vast majority of cases — but
    /// apps that know they're about to release the bus name can use
    /// this to be tidy.
    async fn unregister_app(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<bool> {
        let Some(sender) = header.sender() else {
            return Err(zbus::fdo::Error::Failed(
                "no sender on D-Bus message".into(),
            ));
        };
        Ok(self
            .registry
            .lock()
            .unwrap()
            .remove_owner(&sender.to_string())
            .is_some())
    }

    /// Look up the `app_id` registered for a given D-Bus owner.
    /// Returns the empty string if the owner is not registered — zbus
    /// `Option<String>` round-trips as a `Maybe<s>` which most
    /// bindings handle awkwardly; the empty sentinel is simpler.
    async fn get_app_id(&self, owner: String) -> zbus::fdo::Result<String> {
        Ok(self
            .registry
            .lock()
            .unwrap()
            .by_owner(&owner)
            .map(|a| a.app_id.clone())
            .unwrap_or_default())
    }

    /// List `(app_id, name)` pairs for every currently registered
    /// client. Primarily for debugging — `busctl --user call ...`.
    async fn list_apps(&self) -> zbus::fdo::Result<Vec<(String, String)>> {
        Ok(self
            .registry
            .lock()
            .unwrap()
            .all()
            .into_iter()
            .map(|a| (a.app_id, a.name))
            .collect())
    }
}

async fn serve(
    registry: Arc<Mutex<AppRegistry>>,
    executor: &ThreadPool,
) -> zbus::Result<zbus::Connection> {
    let conn = zbus::Connection::session().await?;
    let iface = AppInterface {
        registry,
        executor: executor.clone(),
    };
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

    fn mk(owner: &str, app_id: &str) -> AppInfo {
        AppInfo {
            app_id: app_id.into(),
            name: format!("Display name for {app_id}"),
            owner: owner.into(),
            declared_actions: vec![],
            permissions: vec![],
        }
    }

    fn mk_with_perms(owner: &str, app_id: &str, permissions: Vec<&str>) -> AppInfo {
        AppInfo {
            app_id: app_id.into(),
            name: format!("Display name for {app_id}"),
            owner: owner.into(),
            declared_actions: vec![],
            permissions: permissions.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn insert_and_lookup_by_owner_and_app_id() {
        let mut r = AppRegistry::default();
        r.insert(mk(":1.10", "org.editor"));
        r.insert(mk(":1.11", "org.terminal"));
        assert_eq!(r.by_owner(":1.10").unwrap().app_id, "org.editor");
        assert_eq!(r.by_app_id("org.terminal").unwrap().owner, ":1.11");
        assert!(r.by_owner(":1.999").is_none());
        assert!(r.by_app_id("unknown").is_none());
    }

    #[test]
    fn insert_replaces_on_same_owner() {
        let mut r = AppRegistry::default();
        r.insert(mk(":1.10", "org.editor"));
        r.insert(AppInfo {
            app_id: "org.editor".into(),
            name: "Editor v2".into(),
            owner: ":1.10".into(),
            declared_actions: vec![DeclaredAction {
                id: "save".into(),
                label: "Save".into(),
                description: String::new(),
            }],
            permissions: vec![],
        });
        let info = r.by_owner(":1.10").unwrap();
        assert_eq!(info.name, "Editor v2");
        assert_eq!(info.declared_actions.len(), 1);
    }

    #[test]
    fn permissions_lookup() {
        let a = mk_with_perms(":1.1", "org.a", vec!["register_global_bindings"]);
        assert!(a.can_register_global_bindings());

        let b = mk_with_perms(":1.2", "org.b", vec!["register_focused_bindings"]);
        assert!(!b.can_register_global_bindings());

        let c = mk(":1.3", "org.c");
        assert!(!c.can_register_global_bindings());
    }

    #[test]
    fn permissions_forward_compat_keeps_unknown() {
        // The registry stores declared permissions verbatim; unknown
        // strings must not be dropped so new compositor versions can
        // consult them without clients needing to upgrade.
        let info = mk_with_perms(":1.4", "org.d", vec!["future_permission"]);
        assert_eq!(info.permissions, vec!["future_permission".to_string()]);
    }

    #[test]
    fn remove_owner_returns_entry() {
        let mut r = AppRegistry::default();
        r.insert(mk(":1.10", "org.editor"));
        let removed = r.remove_owner(":1.10");
        assert!(removed.is_some());
        assert!(r.by_owner(":1.10").is_none());
        // Second remove is a no-op.
        assert!(r.remove_owner(":1.10").is_none());
    }

    #[test]
    fn all_and_is_empty() {
        let mut r = AppRegistry::default();
        assert!(r.is_empty());
        r.insert(mk(":1.1", "a"));
        r.insert(mk(":1.2", "b"));
        assert!(!r.is_empty());
        assert_eq!(r.all().len(), 2);
    }

    #[test]
    fn by_app_id_returns_first_match_only() {
        // Two owners claiming the same app_id — pathological, but the
        // registry keeps both entries and returns whichever HashMap
        // iteration order hits first. What must NOT happen is a
        // panic.
        let mut r = AppRegistry::default();
        r.insert(mk(":1.10", "org.same"));
        r.insert(mk(":1.11", "org.same"));
        let info = r.by_app_id("org.same").unwrap();
        assert!(info.owner == ":1.10" || info.owner == ":1.11");
    }
}
