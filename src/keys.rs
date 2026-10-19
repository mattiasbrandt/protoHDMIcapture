//! Key presses (#23): from the key, its modifiers, whether it is held down
//! and the window's mode, the one action it asks for. `App` only applies it,
//! so every rule of the dispatch is here, where a test reaches it.

use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::level::LevelChange;
use crate::overlay::Page;
use crate::picture::Colour;
use crate::wizard;

/// What the window shows, as far as keys care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The setup wizard's window: every key is the wizard's.
    Wizard,
    /// The viewer, with the overlay's page open (None: closed).
    Viewer(Option<Page>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    Wizard(wizard::Key),
    /// Tab: the overlay's panel opened or closed.
    ToggleOverlay,
    /// Esc: the overlay's page closed.
    CloseOverlay,
    /// The tips page's arrows: the previous (-1) or next (1) source profile.
    CycleTips(i32),
    CycleLevels,
    CycleScaling,
    ResetColour,
    Save,
    ToggleTips,
    CycleDisplay,
    Dashboard,
    Screenshot,
    /// The audio sync moved by this many ms.
    Sync(i32),
    Level(LevelChange),
    /// A colour moved one step down (-1) or up (1).
    Picture(Colour, i32),
}

/// The action a pressed key asks for, or None when it asks for nothing.
/// In order: the wizard takes every key; q quits; Tab, f, m, s and d act
/// once when held down; with ctrl, alt or super only the audio sync keys
/// act; then the overlay's keys, f, d and s, the audio keys and the
/// picture keys.
pub fn action(key: &Key, m: ModifiersState, repeat: bool, mode: Mode) -> Option<Action> {
    let page = match mode {
        // Every key is the wizard's, q included (it may be typed).
        Mode::Wizard => return wizard_key(key, m, repeat).map(Action::Wizard),
        Mode::Viewer(page) => page,
    };
    let is = |k: &str| matches!(key, Key::Character(c) if c == k);
    // q quits, as in view.sh, with or without modifiers.
    if is("q") {
        return Some(Action::Quit);
    }
    // Held down, these act once (the mpv script binds Tab, f and m without
    // repeat; s is a shot per press); the rest repeat, the overlay's keys
    // included, as there.
    let once = matches!(key, Key::Named(NamedKey::Tab)) || ["f", "m", "s", "d"].into_iter().any(is);
    if repeat && once {
        return None;
    }
    // ctrl, alt or super with a key belong to the desktop, or to the sync
    // keys (#12's review found ctrl+alt+m muting).
    if m.intersects(ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SUPER) {
        return audio(key, m);
    }
    overlay(key, page).or_else(|| window(key)).or_else(|| audio(key, m)).or_else(|| picture(key))
}

/// f, d and s: the display mode, the dashboard and a screenshot.
fn window(key: &Key) -> Option<Action> {
    let Key::Character(c) = key else { return None };
    Some(match c.as_str() {
        "f" => Action::CycleDisplay,
        "d" => Action::Dashboard,
        "s" => Action::Screenshot,
        _ => return None,
    })
}

/// Tab, and the keys that only mean something while the overlay is open,
/// as the mpv script binds them.
fn overlay(key: &Key, page: Option<Page>) -> Option<Action> {
    Some(match key {
        Key::Named(NamedKey::Tab) => Action::ToggleOverlay,
        _ if page.is_none() => return None,
        Key::Named(NamedKey::Escape) => Action::CloseOverlay,
        Key::Named(NamedKey::ArrowLeft) if page == Some(Page::Tips) => Action::CycleTips(-1),
        Key::Named(NamedKey::ArrowRight) if page == Some(Page::Tips) => Action::CycleTips(1),
        Key::Character(c) => match c.as_str() {
            "l" => Action::CycleLevels,
            "c" => Action::CycleScaling,
            "r" => Action::ResetColour,
            "w" => Action::Save,
            "t" => Action::ToggleTips,
            _ => return None,
        },
        _ => return None,
    })
}

/// The audio keys as the mpv script binds them: ctrl -/+ (or =) 10 ms,
/// alt -/+ 50 ms, and m, 9, 0 plain (no ctrl, alt or super).
fn audio(key: &Key, m: ModifiersState) -> Option<Action> {
    let Key::Character(c) = key else { return None };
    if m.super_key() {
        return None;
    }
    let step = match (m.control_key(), m.alt_key()) {
        (true, false) => 10,
        (false, true) => 50,
        (false, false) => 0,
        (true, true) => return None,
    };
    Some(match (c.as_str(), step) {
        ("-", s) if s > 0 => Action::Sync(-s),
        ("+" | "=", s) if s > 0 => Action::Sync(s),
        ("m", 0) => Action::Level(LevelChange::Mute),
        ("9", 0) => Action::Level(LevelChange::Volume(-5)),
        ("0", 0) => Action::Level(LevelChange::Volume(5)),
        _ => return None,
    })
}

/// The picture keys, mpv's 1..8.
fn picture(key: &Key) -> Option<Action> {
    let Key::Character(c) = key else { return None };
    let (colour, by) = match c.as_str() {
        "1" => (Colour::Contrast, -1),
        "2" => (Colour::Contrast, 1),
        "3" => (Colour::Brightness, -1),
        "4" => (Colour::Brightness, 1),
        "5" => (Colour::Gamma, -1),
        "6" => (Colour::Gamma, 1),
        "7" => (Colour::Saturation, -1),
        "8" => (Colour::Saturation, 1),
        _ => return None,
    };
    Some(Action::Picture(colour, by))
}

/// A key as the wizard reads it: arrows, Enter, Esc, Backspace and plain
/// characters; held down, only the arrows and Backspace repeat.
fn wizard_key(key: &Key, m: ModifiersState, repeat: bool) -> Option<wizard::Key> {
    let k = match key {
        Key::Named(NamedKey::ArrowUp) => wizard::Key::Up,
        Key::Named(NamedKey::ArrowDown) => wizard::Key::Down,
        Key::Named(NamedKey::ArrowLeft) if m.shift_key() => wizard::Key::ShiftLeft,
        Key::Named(NamedKey::ArrowRight) if m.shift_key() => wizard::Key::ShiftRight,
        Key::Named(NamedKey::ArrowLeft) => wizard::Key::Left,
        Key::Named(NamedKey::ArrowRight) => wizard::Key::Right,
        Key::Named(NamedKey::Backspace) => wizard::Key::Backspace,
        _ if repeat => return None,
        Key::Named(NamedKey::Enter) => wizard::Key::Enter,
        Key::Named(NamedKey::Escape) => wizard::Key::Escape,
        Key::Character(c) if !m.intersects(ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SUPER) => {
            wizard::Key::Char(c.to_string())
        }
        _ => return None,
    };
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWER: Mode = Mode::Viewer(None);
    const PANEL: Mode = Mode::Viewer(Some(Page::Panel));
    const TIPS: Mode = Mode::Viewer(Some(Page::Tips));

    fn key(k: &str) -> Key {
        match k {
            "Tab" => Key::Named(NamedKey::Tab),
            "Esc" => Key::Named(NamedKey::Escape),
            "Enter" => Key::Named(NamedKey::Enter),
            "Left" => Key::Named(NamedKey::ArrowLeft),
            "Right" => Key::Named(NamedKey::ArrowRight),
            "Up" => Key::Named(NamedKey::ArrowUp),
            "Backspace" => Key::Named(NamedKey::Backspace),
            c => Key::Character(c.into()),
        }
    }

    fn press(k: &str, m: ModifiersState, mode: Mode) -> Option<Action> {
        action(&key(k), m, false, mode)
    }

    fn held(k: &str, mode: Mode) -> Option<Action> {
        action(&key(k), ModifiersState::empty(), true, mode)
    }

    const NONE: ModifiersState = ModifiersState::empty();
    const CTRL: ModifiersState = ModifiersState::CONTROL;
    const ALT: ModifiersState = ModifiersState::ALT;
    const SUPER: ModifiersState = ModifiersState::SUPER;
    const SHIFT: ModifiersState = ModifiersState::SHIFT;

    #[test]
    fn audio_keys_as_the_mpv_script_binds_them() {
        use Action::{Level, Sync};
        let table = [
            ("-", CTRL, Some(Sync(-10))),
            ("=", CTRL, Some(Sync(10))),
            ("+", CTRL, Some(Sync(10))),
            ("-", ALT, Some(Sync(-50))),
            ("+", ALT.union(SHIFT), Some(Sync(50))),
            ("-", NONE, None),
            ("-", CTRL.union(SUPER), None),
            ("-", CTRL.union(ALT), None),
            // m, 9 and 0 plain only, as the review found ctrl+alt+m muting
            // (8f49d73).
            ("m", NONE, Some(Level(LevelChange::Mute))),
            ("9", NONE, Some(Level(LevelChange::Volume(-5)))),
            ("0", NONE, Some(Level(LevelChange::Volume(5)))),
            ("m", CTRL, None),
            ("9", CTRL, None),
            ("0", CTRL, None),
            ("m", CTRL.union(ALT), None),
            ("9", CTRL.union(ALT), None),
            ("0", CTRL.union(ALT), None),
            ("m", ALT, None),
            ("m", SUPER, None),
        ];
        for (k, m, want) in table {
            assert_eq!(press(k, m, VIEWER), want, "{k} with {m:?}");
        }
    }

    #[test]
    fn s_then_q_is_a_shot_then_a_quit() {
        assert_eq!(press("s", NONE, VIEWER), Some(Action::Screenshot));
        assert_eq!(press("q", NONE, VIEWER), Some(Action::Quit));
        // q quits with modifiers, held down and with the overlay open.
        assert_eq!(press("q", CTRL, VIEWER), Some(Action::Quit));
        assert_eq!(held("q", PANEL), Some(Action::Quit));
        assert_eq!(press("s", CTRL, VIEWER), None);
        assert_eq!(press("f", NONE, VIEWER), Some(Action::CycleDisplay));
        assert_eq!(press("d", NONE, PANEL), Some(Action::Dashboard));
    }

    #[test]
    fn tab_f_m_s_and_d_act_once_when_held() {
        for k in ["Tab", "f", "m", "s", "d"] {
            assert_eq!(held(k, VIEWER), None, "{k} held");
            assert_eq!(held(k, PANEL), None, "{k} held, the panel open");
        }
        // The rest repeat, the overlay's keys included.
        assert_eq!(held("9", VIEWER), Some(Action::Level(LevelChange::Volume(-5))));
        assert_eq!(held("2", VIEWER), Some(Action::Picture(Colour::Contrast, 1)));
        assert_eq!(held("w", PANEL), Some(Action::Save));
        assert_eq!(held("t", PANEL), Some(Action::ToggleTips));
        assert_eq!(held("Right", TIPS), Some(Action::CycleTips(1)));
        assert_eq!(action(&key("-"), CTRL, true, VIEWER), Some(Action::Sync(-10)));
    }

    #[test]
    fn overlay_keys_only_while_it_is_open() {
        let table = [
            ("l", Action::CycleLevels),
            ("c", Action::CycleScaling),
            ("r", Action::ResetColour),
            ("w", Action::Save),
            ("t", Action::ToggleTips),
            ("Esc", Action::CloseOverlay),
        ];
        for (k, want) in table {
            assert_eq!(press(k, NONE, VIEWER), None, "{k}, the overlay closed");
            assert_eq!(press(k, NONE, PANEL), Some(want.clone()), "{k}, the panel open");
            assert_eq!(press(k, NONE, TIPS), Some(want), "{k}, the tips open");
            assert_eq!(press(k, CTRL, PANEL), None, "ctrl+{k}");
        }
        assert_eq!(press("Tab", NONE, VIEWER), Some(Action::ToggleOverlay));
        assert_eq!(press("Tab", NONE, PANEL), Some(Action::ToggleOverlay));
        assert_eq!(press("Tab", CTRL, VIEWER), None);
        // The arrows only page the tips.
        assert_eq!(press("Left", NONE, TIPS), Some(Action::CycleTips(-1)));
        assert_eq!(press("Left", NONE, PANEL), None);
        assert_eq!(press("Right", NONE, VIEWER), None);
        // The picture and audio keys work with the overlay open too.
        assert_eq!(press("1", NONE, PANEL), Some(Action::Picture(Colour::Contrast, -1)));
        assert_eq!(press("m", NONE, TIPS), Some(Action::Level(LevelChange::Mute)));
    }

    #[test]
    fn picture_keys_plain_only() {
        assert_eq!(press("3", NONE, VIEWER), Some(Action::Picture(Colour::Brightness, -1)));
        assert_eq!(press("8", SHIFT, VIEWER), Some(Action::Picture(Colour::Saturation, 1)));
        for m in [CTRL, ALT, SUPER] {
            assert_eq!(press("3", m, VIEWER), None, "{m:?}");
        }
        assert_eq!(press("x", NONE, VIEWER), None);
    }

    #[test]
    fn the_wizard_takes_every_key() {
        use wizard::Key as W;
        let w = Mode::Wizard;
        // Even q and the viewer's keys: they may be typed.
        for c in ["q", "f", "s", "d", "m", "l", "1", "w"] {
            assert_eq!(press(c, NONE, w), Some(Action::Wizard(W::Char(c.into()))), "{c}");
        }
        assert_eq!(press("q", CTRL, w), None, "ctrl+q is no character the wizard reads, and no quit");
        assert_eq!(press("Tab", NONE, w), None);
        assert_eq!(press("Esc", NONE, w), Some(Action::Wizard(W::Escape)));
        assert_eq!(press("Enter", NONE, w), Some(Action::Wizard(W::Enter)));
        assert_eq!(press("Left", SHIFT, w), Some(Action::Wizard(W::ShiftLeft)));
        assert_eq!(press("Left", NONE, w), Some(Action::Wizard(W::Left)));
        // Held down, only the arrows and Backspace repeat.
        assert_eq!(held("Up", w), Some(Action::Wizard(W::Up)));
        assert_eq!(held("Backspace", w), Some(Action::Wizard(W::Backspace)));
        assert_eq!(held("Enter", w), None);
        assert_eq!(held("a", w), None);
    }
}
