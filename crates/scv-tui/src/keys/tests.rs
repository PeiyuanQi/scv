//! Unit tests for `src/keys.rs`.

use super::*;

fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn the_defaults_bind_every_action_to_a_distinct_key() {
    let (keys, warnings) = KeyMap::with(&BTreeMap::new());
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        keys.action(press(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        Some(Action::FocusSidebar)
    );
    assert_eq!(
        keys.action(press(KeyCode::Char('3'), KeyModifiers::ALT)),
        Some(Action::Tab(3))
    );
    assert_eq!(
        keys.action(press(KeyCode::F(1), KeyModifiers::NONE)),
        Some(Action::Help)
    );
    // Plain typing is never an action.
    assert_eq!(
        keys.action(press(KeyCode::Char('g'), KeyModifiers::NONE)),
        None
    );
    assert_eq!(keys.label(Action::NextTab), "Alt+.");
    for action in Action::all() {
        assert_ne!(keys.label(action), "unbound", "{}", action.name());
    }
}

#[test]
fn the_users_bindings_replace_defaults_and_bad_ones_keep_them_with_a_warning() {
    let configured = BTreeMap::from([
        ("next_tab".to_owned(), "ctrl+pagedown".to_owned()),
        ("help".to_owned(), "f13".to_owned()),
        ("split".to_owned(), "v".to_owned()),
        ("teleport".to_owned(), "f9".to_owned()),
    ]);
    let (keys, warnings) = KeyMap::with(&configured);
    assert_eq!(
        keys.action(press(KeyCode::PageDown, KeyModifiers::CONTROL)),
        Some(Action::NextTab)
    );
    assert_eq!(
        keys.action(press(KeyCode::Char('.'), KeyModifiers::ALT)),
        None
    );
    // Unusable keys keep the default, and the TUI says so.
    assert_eq!(
        keys.action(press(KeyCode::F(1), KeyModifiers::NONE)),
        Some(Action::Help)
    );
    assert_eq!(
        keys.action(press(KeyCode::Char('v'), KeyModifiers::ALT)),
        Some(Action::Split)
    );
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(warnings.iter().any(|warning| warning.contains("teleport")));
}

#[test]
fn a_configured_key_wins_over_another_actions_default() {
    let configured = BTreeMap::from([("split".to_owned(), "ctrl+g".to_owned())]);
    let (keys, warnings) = KeyMap::with(&configured);
    assert_eq!(
        keys.action(press(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        Some(Action::Split)
    );
    assert_eq!(keys.label(Action::FocusSidebar), "unbound");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("focus_sidebar"), "{warnings:?}");
}

#[test]
fn a_refused_binding_keeps_its_default_and_the_composers_keys_stay_its_own() {
    let configured = BTreeMap::from([
        // The sidebar's default, taken by the user's own binding first.
        ("split".to_owned(), "alt+s".to_owned()),
        // Configured twice: the later one keeps its default.
        ("notifications".to_owned(), "alt+s".to_owned()),
        // Keys a chat needs to send, cancel, and quit.
        ("help".to_owned(), "enter".to_owned()),
        ("new_tab".to_owned(), "ctrl+c".to_owned()),
    ]);
    let (keys, warnings) = KeyMap::with(&configured);
    assert_eq!(
        keys.action(press(KeyCode::Char('s'), KeyModifiers::ALT)),
        Some(Action::Split)
    );
    assert_eq!(keys.label(Action::Notifications), "Alt+N");
    assert_eq!(keys.label(Action::Help), "F1");
    assert_eq!(keys.label(Action::NewTab), "Ctrl+T");
    assert_eq!(keys.label(Action::ToggleSidebar), "unbound");
    assert_eq!(
        keys.action(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(keys.action(press(KeyCode::Enter, KeyModifiers::NONE)), None);
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("chat or agent view uses")),
        "{warnings:?}"
    );
}

#[test]
fn key_specifications_parse_like_people_write_them() {
    let parsed = |spec: &str| Key::parse(spec);
    // Terminals send shift+tab as BackTab, which is how it matches.
    assert_eq!(
        parsed("Ctrl+Shift+Tab"),
        Some(Key {
            code: KeyCode::BackTab,
            modifiers: KeyModifiers::CONTROL
        })
    );
    assert_eq!(
        Key::of(press(
            KeyCode::BackTab,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        parsed("ctrl+shift+tab").unwrap()
    );
    assert_eq!(parsed("ctrl+shift+x"), parsed("ctrl+x"));
    assert_eq!(
        parsed("alt++"),
        Some(Key {
            code: KeyCode::Char('+'),
            modifiers: KeyModifiers::ALT
        })
    );
    assert_eq!(parsed("meta+X").unwrap().code, KeyCode::Char('x'));
    for bad in [
        "",
        "hyper+x",
        "f0",
        "f13",
        "x",
        "shift+x",
        "ctrl+",
        "ctrl+enterr",
        "enter",
        "esc",
        "shift+tab",
        "pagedown",
    ] {
        assert_eq!(parsed(bad), None, "{bad}");
    }
    // A pressed capital letter is the same key as its binding.
    assert_eq!(
        Key::of(press(
            KeyCode::Char('T'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )),
        Key::parse("ctrl+t").unwrap()
    );
}
