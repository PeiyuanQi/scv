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
fn a_key_taken_twice_unbinds_the_later_action() {
    let configured = BTreeMap::from([("split".to_owned(), "ctrl+g".to_owned())]);
    let (keys, warnings) = KeyMap::with(&configured);
    assert_eq!(
        keys.action(press(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        Some(Action::FocusSidebar)
    );
    assert_eq!(keys.label(Action::Split), "unbound");
    assert_eq!(warnings.len(), 1);
}

#[test]
fn key_specifications_parse_like_people_write_them() {
    let parsed = |spec: &str| Key::parse(spec);
    assert_eq!(
        parsed("Ctrl+Shift+Tab"),
        Some(Key {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT
        })
    );
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
