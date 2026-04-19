//! Unified keybinding resolver.
//!
//! The compositor has three sources of bindings today:
//! 1. Hardcoded system bindings (reserved, not rebindable).
//! 2. `[keybindings]` in `compositor.toml` (the user's static map).
//! 3. Dynamic registrations on the `org.lunaris.InputManager1` D-Bus
//!    service (per-app and per-process).
//!
//! A fourth source — `compositor.d/keybindings.d/*.toml` module
//! fragments — is loaded into the same static slot as (2) by the TOML
//! loader, so this module does not need to distinguish them at
//! dispatch time.
//!
//! The resolver is a pure-logic helper that knows *nothing* about D-Bus
//! or file I/O. It receives snapshots of the static bindings (at config
//! reload) and a shared handle to the dynamic bindings map (owned by
//! [`crate::dbus::input_manager::InputManagerState`]). Keep it that way
//! — tests become trivial and the hot input path stays allocation-free.
//!
//! ## Precedence
//!
//! Higher precedence wins. On a tie within the same scope the first
//! matching entry wins.
//!
//! | Scope       | Source                                            |
//! | ----------- | ------------------------------------------------- |
//! | System      | compositor-reserved, constructed by the caller    |
//! | Shell       | static bindings flagged as shell-overlay events   |
//! | User        | `[keybindings]` in `compositor.toml`              |
//! | Module      | `compositor.d/keybindings.d/*.toml` fragments     |
//! | AppGlobal   | D-Bus registrations with `scope = "app_global"`   |
//! | AppFocused  | D-Bus registrations with `scope = "app_focused"`, |
//! |             | fired only when `focused_app_id` matches          |
//!
//! The resolver's `resolve` function walks scopes top-to-bottom and
//! stops at the first hit — there is no fall-through.

use std::sync::{Arc, Mutex, RwLock};

use crate::{
    config::{KeyBinding, KeyBindingModifiers, parse_keybinding},
    dbus::input_manager::DynamicBindings,
};

/// Scope from which a matched binding originated. Kept simple and
/// string-free so the dispatch path can match on a plain enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum BindingScope {
    System = 0,
    Shell = 1,
    User = 2,
    Module = 3,
    AppGlobal = 4,
    AppFocused = 5,
}

/// A static binding that the resolver can serve from in-memory caches.
/// The compositor rebuilds the vector each time the TOML reloads.
#[derive(Clone, Debug)]
pub struct StaticBinding {
    pub modifiers: KeyBindingModifiers,
    /// Raw key name, matched via `keysym_from_str` at key time. Kept as
    /// a `String` (not a parsed keysym) so the struct stays `Clone`-able
    /// and cheap to rebuild on hot-reload.
    pub key: String,
    pub action: String,
    pub scope: BindingScope,
}

impl StaticBinding {
    /// Convenience constructor from the structured [`KeyBinding`] the
    /// TOML loader produces.
    pub fn from_toml(kb: &KeyBinding, scope: BindingScope) -> Self {
        Self {
            modifiers: kb.modifiers,
            key: kb.key.clone(),
            action: kb.action.clone(),
            scope,
        }
    }

    /// Try to convert an opaque accelerator string (`"Super+Shift+H"`)
    /// into a [`StaticBinding`]. Returns `None` for unparseable input.
    pub fn from_accelerator(accelerator: &str, action: String, scope: BindingScope) -> Option<Self> {
        let (modifiers, key) = parse_keybinding(accelerator)?;
        Some(Self {
            modifiers,
            key,
            action,
            scope,
        })
    }
}

/// A successful match from the resolver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedBinding {
    pub action: String,
    pub scope: BindingScope,
    /// Only set for dynamic matches, used by the dispatcher to route
    /// the `BindingInvoked` signal back to the registering client.
    pub owner: Option<String>,
}

/// Central resolver. Cheaply clonable — everything inside is
/// reference-counted — so you can hand a clone to each code path that
/// needs it instead of passing references through.
#[derive(Clone, Debug)]
pub struct BindingResolver {
    /// Static bindings, sorted by scope precedence (ascending). The
    /// caller is responsible for providing a sorted vector; the
    /// resolver does not re-sort on every lookup because that would
    /// allocate on the hot path.
    static_bindings: Arc<RwLock<Vec<StaticBinding>>>,
    /// Dynamic bindings. Shared with `InputManagerState` so that a
    /// successful D-Bus `RegisterBinding` becomes immediately visible
    /// to the dispatcher without an extra copy.
    dynamic: Arc<Mutex<DynamicBindings>>,
    /// `Some(app_id)` of the currently focused toplevel, or `None` if
    /// no Wayland client has keyboard focus.
    focused_app_id: Arc<RwLock<Option<String>>>,
}

impl BindingResolver {
    pub fn new(dynamic: Arc<Mutex<DynamicBindings>>) -> Self {
        Self {
            static_bindings: Arc::new(RwLock::new(Vec::new())),
            dynamic,
            focused_app_id: Arc::new(RwLock::new(None)),
        }
    }

    /// Replace the static binding set. Caller should order entries by
    /// descending scope precedence (System first, Module last); a
    /// stable sort is applied to be safe.
    pub fn set_static_bindings(&self, mut bindings: Vec<StaticBinding>) {
        bindings.sort_by_key(|b| b.scope);
        *self.static_bindings.write().unwrap() = bindings;
    }

    /// Update which app currently holds keyboard focus. Pass `None` to
    /// signal "no focus" — in that state `app_focused` dynamic bindings
    /// do not fire, which matches the spec.
    pub fn set_focused_app(&self, app_id: Option<String>) {
        *self.focused_app_id.write().unwrap() = app_id;
    }

    /// Current focused app id, mostly for tests and introspection.
    pub fn focused_app_id(&self) -> Option<String> {
        self.focused_app_id.read().unwrap().clone()
    }

    /// Look up `(modifiers, key)` against all sources and return the
    /// highest-precedence match.
    pub fn resolve(
        &self,
        modifiers: &KeyBindingModifiers,
        key: &str,
    ) -> Option<ResolvedBinding> {
        // 1. Static (already sorted by scope).
        {
            let statics = self.static_bindings.read().unwrap();
            for b in statics.iter() {
                if b.modifiers == *modifiers && b.key.eq_ignore_ascii_case(key) {
                    return Some(ResolvedBinding {
                        action: b.action.clone(),
                        scope: b.scope,
                        owner: None,
                    });
                }
            }
        }

        // 2. Dynamic: AppGlobal first (fires regardless of focus).
        let focused = self.focused_app_id.read().unwrap().clone();
        let dynamic = self.dynamic.lock().unwrap();
        for entry in dynamic.by_owner.values().flatten() {
            if entry.scope != "app_global" {
                continue;
            }
            if let Some(parsed) = parse_keybinding(&entry.binding) {
                if parsed.0 == *modifiers && parsed.1.eq_ignore_ascii_case(key) {
                    return Some(ResolvedBinding {
                        action: entry.action.clone(),
                        scope: BindingScope::AppGlobal,
                        owner: Some(entry.owner.clone()),
                    });
                }
            }
        }

        // 3. Dynamic: AppFocused — only fires when the focused app
        //    matches the registration's declared app_id.
        let Some(focused_app) = focused else {
            return None;
        };
        for entry in dynamic.by_owner.values().flatten() {
            if entry.scope != "app_focused" {
                continue;
            }
            if entry.app_id != focused_app {
                continue;
            }
            if let Some(parsed) = parse_keybinding(&entry.binding) {
                if parsed.0 == *modifiers && parsed.1.eq_ignore_ascii_case(key) {
                    return Some(ResolvedBinding {
                        action: entry.action.clone(),
                        scope: BindingScope::AppFocused,
                        owner: Some(entry.owner.clone()),
                    });
                }
            }
        }

        None
    }

    /// Return every match for `(modifiers, key)` across all scopes.
    /// Used by Settings for conflict reporting — *not* the dispatch
    /// path, which stops at the first match.
    pub fn list_conflicts(
        &self,
        modifiers: &KeyBindingModifiers,
        key: &str,
    ) -> Vec<ResolvedBinding> {
        let mut out = Vec::new();

        {
            let statics = self.static_bindings.read().unwrap();
            for b in statics.iter() {
                if b.modifiers == *modifiers && b.key.eq_ignore_ascii_case(key) {
                    out.push(ResolvedBinding {
                        action: b.action.clone(),
                        scope: b.scope,
                        owner: None,
                    });
                }
            }
        }

        let dynamic = self.dynamic.lock().unwrap();
        for entry in dynamic.by_owner.values().flatten() {
            if let Some(parsed) = parse_keybinding(&entry.binding) {
                if parsed.0 == *modifiers && parsed.1.eq_ignore_ascii_case(key) {
                    let scope = match entry.scope.as_str() {
                        "app_global" => BindingScope::AppGlobal,
                        _ => BindingScope::AppFocused,
                    };
                    out.push(ResolvedBinding {
                        action: entry.action.clone(),
                        scope,
                        owner: Some(entry.owner.clone()),
                    });
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::input_manager::BindingInfo;

    fn mods(super_k: bool, shift: bool, ctrl: bool, alt: bool) -> KeyBindingModifiers {
        KeyBindingModifiers {
            super_key: super_k,
            shift,
            ctrl,
            alt,
        }
    }

    fn fresh() -> BindingResolver {
        BindingResolver::new(Arc::new(Mutex::new(DynamicBindings::default())))
    }

    fn insert_dynamic(r: &BindingResolver, info: BindingInfo) {
        r.dynamic.lock().unwrap().insert(info);
    }

    #[test]
    fn resolves_static_user_binding() {
        let r = fresh();
        r.set_static_bindings(vec![StaticBinding {
            modifiers: mods(true, false, false, false),
            key: "h".into(),
            action: "focus_left".into(),
            scope: BindingScope::User,
        }]);
        let got = r.resolve(&mods(true, false, false, false), "h").unwrap();
        assert_eq!(got.action, "focus_left");
        assert_eq!(got.scope, BindingScope::User);
        assert!(got.owner.is_none());
    }

    #[test]
    fn resolves_case_insensitively_on_key() {
        let r = fresh();
        r.set_static_bindings(vec![StaticBinding {
            modifiers: mods(true, false, false, false),
            key: "H".into(),
            action: "focus_left".into(),
            scope: BindingScope::User,
        }]);
        assert!(r.resolve(&mods(true, false, false, false), "h").is_some());
        assert!(r.resolve(&mods(true, false, false, false), "H").is_some());
    }

    #[test]
    fn static_wins_over_dynamic_for_same_binding() {
        let r = fresh();
        r.set_static_bindings(vec![StaticBinding {
            modifiers: mods(false, false, true, false),
            key: "s".into(),
            action: "user_save".into(),
            scope: BindingScope::User,
        }]);
        insert_dynamic(
            &r,
            BindingInfo {
                binding: "Ctrl+S".into(),
                action: "editor:save".into(),
                scope: "app_global".into(),
                owner: ":1.42".into(),
                app_id: "".into(),
            },
        );
        let got = r.resolve(&mods(false, false, true, false), "s").unwrap();
        assert_eq!(got.scope, BindingScope::User);
        assert_eq!(got.action, "user_save");
    }

    #[test]
    fn system_wins_over_user() {
        let r = fresh();
        r.set_static_bindings(vec![
            StaticBinding {
                modifiers: mods(false, false, false, true),
                key: "F4".into(),
                action: "user_action".into(),
                scope: BindingScope::User,
            },
            StaticBinding {
                modifiers: mods(false, false, false, true),
                key: "F4".into(),
                action: "system_action".into(),
                scope: BindingScope::System,
            },
        ]);
        let got = r.resolve(&mods(false, false, false, true), "F4").unwrap();
        assert_eq!(got.scope, BindingScope::System);
        assert_eq!(got.action, "system_action");
    }

    #[test]
    fn dynamic_app_global_fires_without_focus() {
        let r = fresh();
        insert_dynamic(
            &r,
            BindingInfo {
                binding: "Super+G".into(),
                action: "screencap".into(),
                scope: "app_global".into(),
                owner: ":1.10".into(),
                app_id: "".into(),
            },
        );
        let got = r.resolve(&mods(true, false, false, false), "g").unwrap();
        assert_eq!(got.scope, BindingScope::AppGlobal);
        assert_eq!(got.owner.as_deref(), Some(":1.10"));
    }

    #[test]
    fn dynamic_app_focused_requires_matching_focus() {
        let r = fresh();
        insert_dynamic(
            &r,
            BindingInfo {
                binding: "Ctrl+S".into(),
                action: "editor:save".into(),
                scope: "app_focused".into(),
                owner: ":1.20".into(),
                app_id: "org.example.Editor".into(),
            },
        );
        // No focus -> no match.
        assert!(r.resolve(&mods(false, false, true, false), "s").is_none());
        // Wrong app -> no match.
        r.set_focused_app(Some("org.other.App".into()));
        assert!(r.resolve(&mods(false, false, true, false), "s").is_none());
        // Right app -> match.
        r.set_focused_app(Some("org.example.Editor".into()));
        let got = r.resolve(&mods(false, false, true, false), "s").unwrap();
        assert_eq!(got.scope, BindingScope::AppFocused);
    }

    #[test]
    fn dynamic_app_global_preempts_app_focused() {
        let r = fresh();
        insert_dynamic(
            &r,
            BindingInfo {
                binding: "Super+Shift+X".into(),
                action: "global:action".into(),
                scope: "app_global".into(),
                owner: ":1.30".into(),
                app_id: "".into(),
            },
        );
        insert_dynamic(
            &r,
            BindingInfo {
                binding: "Super+Shift+X".into(),
                action: "editor:action".into(),
                scope: "app_focused".into(),
                owner: ":1.31".into(),
                app_id: "org.editor".into(),
            },
        );
        r.set_focused_app(Some("org.editor".into()));
        let got = r.resolve(&mods(true, true, false, false), "x").unwrap();
        assert_eq!(got.scope, BindingScope::AppGlobal);
        assert_eq!(got.action, "global:action");
    }

    #[test]
    fn list_conflicts_reports_all_matches() {
        let r = fresh();
        r.set_static_bindings(vec![StaticBinding {
            modifiers: mods(true, false, false, false),
            key: "q".into(),
            action: "close_window".into(),
            scope: BindingScope::User,
        }]);
        insert_dynamic(
            &r,
            BindingInfo {
                binding: "Super+Q".into(),
                action: "editor:quit".into(),
                scope: "app_global".into(),
                owner: ":1.50".into(),
                app_id: "".into(),
            },
        );
        let c = r.list_conflicts(&mods(true, false, false, false), "q");
        assert_eq!(c.len(), 2);
        assert!(c.iter().any(|r| r.scope == BindingScope::User));
        assert!(c.iter().any(|r| r.scope == BindingScope::AppGlobal));
    }

    #[test]
    fn from_accelerator_parses_and_assigns_scope() {
        let b = StaticBinding::from_accelerator(
            "Super+Shift+Space",
            "toggle_floating".into(),
            BindingScope::User,
        )
        .unwrap();
        assert!(b.modifiers.super_key && b.modifiers.shift);
        assert_eq!(b.action, "toggle_floating");
        assert_eq!(b.key, "Space");
        // parse_keybinding treats any trailing token as the key name,
        // so a single-word input yields no modifiers + the word as key.
        // Exercise that explicit contract rather than asserting `None`.
        let bare = StaticBinding::from_accelerator(
            "Minus",
            "scratchpad".into(),
            BindingScope::User,
        )
        .unwrap();
        assert_eq!(bare.modifiers, mods(false, false, false, false));
        assert_eq!(bare.key, "Minus");
    }

    #[test]
    fn no_match_when_modifiers_differ() {
        let r = fresh();
        r.set_static_bindings(vec![StaticBinding {
            modifiers: mods(true, false, false, false),
            key: "h".into(),
            action: "focus_left".into(),
            scope: BindingScope::User,
        }]);
        // Super released -> no match.
        assert!(r.resolve(&mods(false, false, false, false), "h").is_none());
        // Extra Shift -> no match (exact modifier equality).
        assert!(r.resolve(&mods(true, true, false, false), "h").is_none());
    }

    #[test]
    fn focused_app_id_round_trip() {
        let r = fresh();
        assert!(r.focused_app_id().is_none());
        r.set_focused_app(Some("com.example".into()));
        assert_eq!(r.focused_app_id(), Some("com.example".into()));
        r.set_focused_app(None);
        assert!(r.focused_app_id().is_none());
    }
}
