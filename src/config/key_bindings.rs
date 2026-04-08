use cosmic_settings_config::shortcuts::State as KeyState;
use cosmic_settings_config::shortcuts::{self, Modifiers};
use cosmic_settings_config::shortcuts::action::{Direction, FocusDirection, ResizeDirection, ResizeEdge};
use smithay::input::keyboard::ModifiersState;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Behaviors managed internally by cosmic-comp.
    Private(PrivateAction),
    /// Behaviors managed via cosmic-settings.
    Shortcut(shortcuts::Action),
}

#[derive(Clone, Debug, Eq, PartialEq)]
// Behaviors which are internally defined and emitted.
pub enum PrivateAction {
    Escape,
    Resizing(
        ResizeDirection,
        ResizeEdge,
        shortcuts::State,
    ),
    /// Toggle the scratchpad (show/hide/cycle). Handler in Phase 3.
    ScratchpadToggle,
    /// Move focused window to scratchpad. Handler in Phase 3.
    ScratchpadMove,
    /// Toggle monocle mode for current workspace. Handler in Phase 4.
    ToggleMonocle,
}

/// Map a TOML keybinding action string to an `Action` enum.
///
/// Returns `None` for unknown action strings.
pub fn action_from_str(s: &str) -> Option<Action> {
    Some(match s {
        // Tiling
        "toggle_tiling" => Action::Shortcut(shortcuts::Action::ToggleTiling),
        "toggle_window_floating" => Action::Shortcut(shortcuts::Action::ToggleWindowFloating),

        // Focus
        "focus_left" => Action::Shortcut(shortcuts::Action::Focus(FocusDirection::Left)),
        "focus_right" => Action::Shortcut(shortcuts::Action::Focus(FocusDirection::Right)),
        "focus_up" => Action::Shortcut(shortcuts::Action::Focus(FocusDirection::Up)),
        "focus_down" => Action::Shortcut(shortcuts::Action::Focus(FocusDirection::Down)),

        // Move
        "move_left" => Action::Shortcut(shortcuts::Action::Move(Direction::Left)),
        "move_right" => Action::Shortcut(shortcuts::Action::Move(Direction::Right)),
        "move_up" => Action::Shortcut(shortcuts::Action::Move(Direction::Up)),
        "move_down" => Action::Shortcut(shortcuts::Action::Move(Direction::Down)),

        // Resize (handled via PrivateAction::Resizing)
        "resize_shrink_width" => Action::Private(PrivateAction::Resizing(ResizeDirection::Inwards, ResizeEdge::Right, shortcuts::State::Pressed)),
        "resize_grow_width" => Action::Private(PrivateAction::Resizing(ResizeDirection::Outwards, ResizeEdge::Right, shortcuts::State::Pressed)),
        "resize_shrink_height" => Action::Private(PrivateAction::Resizing(ResizeDirection::Inwards, ResizeEdge::Bottom, shortcuts::State::Pressed)),
        "resize_grow_height" => Action::Private(PrivateAction::Resizing(ResizeDirection::Outwards, ResizeEdge::Bottom, shortcuts::State::Pressed)),

        // Fullscreen
        "fullscreen" | "toggle_fullscreen" => Action::Shortcut(shortcuts::Action::Fullscreen),

        // Swap
        "swap_window" => Action::Shortcut(shortcuts::Action::SwapWindow),

        // Workspace
        "workspace_next" => Action::Shortcut(shortcuts::Action::NextWorkspace),
        "workspace_prev" => Action::Shortcut(shortcuts::Action::PreviousWorkspace),

        // Window management
        "close_window" => Action::Shortcut(shortcuts::Action::Close),
        "maximize" | "toggle_maximize" => Action::Shortcut(shortcuts::Action::Maximize),
        "minimize" => Action::Shortcut(shortcuts::Action::Minimize),

        // Lunaris extensions
        "scratchpad_toggle" => Action::Private(PrivateAction::ScratchpadToggle),
        "scratchpad_move" => Action::Private(PrivateAction::ScratchpadMove),
        "toggle_monocle" => Action::Private(PrivateAction::ToggleMonocle),

        _ => return None,
    })
}

/// Map a key name string to a Keysym for matching.
///
/// Supports single characters ("H", "T"), special names ("Minus", "Space",
/// "Return", "Tab"), and F-keys ("F1"-"F12").
pub fn keysym_from_str(key: &str) -> Option<smithay::input::keyboard::Keysym> {
    use smithay::input::keyboard::Keysym;

    // Single ASCII character.
    if key.len() == 1 {
        let c = key.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            // XKB keysyms for a-z are 0x61-0x7a.
            return Some(Keysym::new(c.to_ascii_lowercase() as u32));
        }
        if c.is_ascii_digit() {
            // XKB keysyms for 0-9 are 0x30-0x39.
            return Some(Keysym::new(c as u32));
        }
    }

    // Named keys.
    Some(match key.to_lowercase().as_str() {
        "space" => Keysym::space,
        "return" | "enter" => Keysym::Return,
        "tab" => Keysym::Tab,
        "escape" | "esc" => Keysym::Escape,
        "backspace" => Keysym::BackSpace,
        "delete" | "del" => Keysym::Delete,
        "minus" => Keysym::minus,
        "plus" | "equal" => Keysym::equal,
        "bracketleft" => Keysym::bracketleft,
        "bracketright" => Keysym::bracketright,
        "semicolon" => Keysym::semicolon,
        "comma" => Keysym::comma,
        "period" => Keysym::period,
        "slash" => Keysym::slash,
        "backslash" => Keysym::backslash,
        "left" => Keysym::Left,
        "right" => Keysym::Right,
        "up" => Keysym::Up,
        "down" => Keysym::Down,
        "home" => Keysym::Home,
        "end" => Keysym::End,
        "pageup" | "page_up" => Keysym::Page_Up,
        "pagedown" | "page_down" => Keysym::Page_Down,
        "f1" => Keysym::F1,
        "f2" => Keysym::F2,
        "f3" => Keysym::F3,
        "f4" => Keysym::F4,
        "f5" => Keysym::F5,
        "f6" => Keysym::F6,
        "f7" => Keysym::F7,
        "f8" => Keysym::F8,
        "f9" => Keysym::F9,
        "f10" => Keysym::F10,
        "f11" => Keysym::F11,
        "f12" => Keysym::F12,
        _ => return None,
    })
}

/// Convert `cosmic_settings_config::shortcuts::State` to `smithay::backend::input::KeyState`.
pub fn cosmic_keystate_to_smithay(value: KeyState) -> smithay::backend::input::KeyState {
    match value {
        KeyState::Pressed => smithay::backend::input::KeyState::Pressed,
        KeyState::Released => smithay::backend::input::KeyState::Released,
    }
}

/// Convert `smithay::backend::input::KeyState` to `cosmic_settings_config::shortcuts::State`.
pub fn cosmic_keystate_from_smithay(value: smithay::backend::input::KeyState) -> KeyState {
    match value {
        smithay::backend::input::KeyState::Pressed => KeyState::Pressed,
        smithay::backend::input::KeyState::Released => KeyState::Released,
    }
}

/// Compare `cosmic_settings_config::shortcuts::Modifiers` to `smithay::input::keyboard::ModifiersState`.
pub fn cosmic_modifiers_eq_smithay(this: &Modifiers, other: &ModifiersState) -> bool {
    this.ctrl == other.ctrl
        && this.alt == other.alt
        && this.shift == other.shift
        && this.logo == other.logo
}

/// Convert `smithay::input::keyboard::ModifiersState` to `cosmic_settings_config::shortcuts::Modifiers`
pub fn cosmic_modifiers_from_smithay(value: ModifiersState) -> Modifiers {
    Modifiers {
        ctrl: value.ctrl,
        alt: value.alt,
        shift: value.shift,
        logo: value.logo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_from_str_known() {
        assert!(matches!(
            action_from_str("toggle_tiling"),
            Some(Action::Shortcut(shortcuts::Action::ToggleTiling))
        ));
        assert!(matches!(
            action_from_str("focus_left"),
            Some(Action::Shortcut(shortcuts::Action::Focus(FocusDirection::Left)))
        ));
        assert!(matches!(
            action_from_str("scratchpad_toggle"),
            Some(Action::Private(PrivateAction::ScratchpadToggle))
        ));
        assert!(matches!(
            action_from_str("toggle_monocle"),
            Some(Action::Private(PrivateAction::ToggleMonocle))
        ));
    }

    #[test]
    fn test_action_from_str_unknown() {
        assert!(action_from_str("nonexistent").is_none());
        assert!(action_from_str("").is_none());
    }

    #[test]
    fn test_keysym_from_str() {
        // Single letters -> lowercase keysym.
        assert!(keysym_from_str("H").is_some());
        assert!(keysym_from_str("T").is_some());
        // Digits.
        assert!(keysym_from_str("1").is_some());
        // Named keys.
        assert!(keysym_from_str("Minus").is_some());
        assert!(keysym_from_str("Space").is_some());
        assert!(keysym_from_str("F1").is_some());
        // Unknown.
        assert!(keysym_from_str("NotAKey").is_none());
    }
}
