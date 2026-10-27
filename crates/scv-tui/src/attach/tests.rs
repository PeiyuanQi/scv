//! Unit tests for `src/attach.rs`.

use scv_protocol::AttachRow;

use super::*;

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn page(id: &str, generation: u64, full: bool, rows: Vec<(u16, &str)>) -> AttachPage {
    AttachPage {
        id: id.into(),
        session: "s".into(),
        conversation: "codex-2".into(),
        agent: "codex".into(),
        run: Some("codex-3f9a2c".into()),
        generation,
        cols: 40,
        rows: 4,
        lines: rows
            .into_iter()
            .map(|(row, text)| AttachRow {
                row,
                spans: vec![AttachSpan {
                    text: text.into(),
                    ..AttachSpan::default()
                }],
            })
            .collect(),
        full,
        cursor: Some([1, 2]),
        application_cursor: false,
        bracketed_paste: false,
        ended: false,
        exit: None,
        note: None,
    }
}

fn text(attached: &Attached) -> Vec<String> {
    attached
        .lines()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

#[test]
fn keys_are_the_bytes_a_terminal_sends() {
    let none = KeyModifiers::NONE;
    let control = KeyModifiers::CONTROL;
    for (pressed, application, bytes) in [
        (key(KeyCode::Char('a'), none), false, "a"),
        (key(KeyCode::Char('é'), none), false, "é"),
        (key(KeyCode::Char('c'), control), false, "\x03"),
        (
            key(KeyCode::Char('T'), control | KeyModifiers::SHIFT),
            false,
            "\x14",
        ),
        (key(KeyCode::Char(' '), control), false, "\0"),
        (key(KeyCode::Char('['), control), false, "\x1b"),
        (key(KeyCode::Char('x'), KeyModifiers::ALT), false, "\x1bx"),
        (key(KeyCode::Enter, none), false, "\r"),
        (key(KeyCode::Tab, none), false, "\t"),
        (key(KeyCode::BackTab, KeyModifiers::SHIFT), false, "\x1b[Z"),
        (key(KeyCode::Backspace, none), false, "\x7f"),
        (key(KeyCode::Esc, none), false, "\x1b"),
        (key(KeyCode::Up, none), false, "\x1b[A"),
        (key(KeyCode::Up, none), true, "\x1bOA"),
        (key(KeyCode::Left, control), true, "\x1b[1;5D"),
        (key(KeyCode::Home, none), false, "\x1b[H"),
        (key(KeyCode::Delete, none), false, "\x1b[3~"),
        (
            key(KeyCode::PageUp, KeyModifiers::SHIFT),
            false,
            "\x1b[5;2~",
        ),
        (key(KeyCode::F(1), none), false, "\x1bOP"),
        (key(KeyCode::F(5), none), false, "\x1b[15~"),
        (key(KeyCode::F(12), none), false, "\x1b[24~"),
    ] {
        assert_eq!(
            encode_key(pressed, application).as_deref(),
            Some(bytes),
            "{pressed:?}"
        );
    }
    assert_eq!(encode_key(key(KeyCode::CapsLock, none), false), None);
}

#[test]
fn ctrl_close_bracket_detaches_however_the_terminal_reports_it() {
    assert!(is_detach(key(KeyCode::Char(']'), KeyModifiers::CONTROL)));
    // Without enhanced keyboard reporting the byte reads as Ctrl+5.
    assert!(is_detach(key(KeyCode::Char('5'), KeyModifiers::CONTROL)));
    assert!(!is_detach(key(KeyCode::Char(']'), KeyModifiers::NONE)));
    assert!(!is_detach(key(KeyCode::Char('d'), KeyModifiers::CONTROL)));
}

#[test]
fn a_paste_is_bracketed_when_asked_and_cannot_end_itself_early() {
    assert_eq!(encode_paste("a\nb\r\nc", false), "a\rb\rc");
    assert_eq!(
        encode_paste("rm -rf x\x1b[201~\ny", true),
        "\x1b[200~rm -rf x\ny\x1b[201~"
    );
}

#[test]
fn pages_fill_the_screen_and_only_changed_rows_replace_it() {
    let mut attached = Attached::new(page("at-1", 3, true, vec![(0, "$ codex"), (1, "ready")]));
    assert_eq!(text(&attached), ["$ codex", "ready", "", ""]);
    attached.apply(page("at-1", 4, false, vec![(1, "working"), (3, "status")]));
    assert_eq!(text(&attached), ["$ codex", "working", "", "status"]);
    attached.apply(page("at-1", 5, true, vec![(2, "fresh")]));
    assert_eq!(text(&attached), ["", "", "fresh", ""]);
    assert_eq!(attached.cursor, Some([1, 2]));
}

#[test]
fn control_characters_from_the_daemon_are_drawn_inert() {
    let attached = Attached::new(page("at-1", 1, true, vec![(0, "a\x1b[2Jb\x07c")]));
    let shown = text(&attached).remove(0);
    assert!(
        !shown.contains('\x1b') && !shown.contains('\x07'),
        "{shown:?}"
    );
    assert!(shown.starts_with('a') && shown.ends_with('c'), "{shown:?}");
}

#[test]
fn one_read_is_in_flight_and_typed_keys_go_with_the_next() {
    let mut attached = Attached::new(page("at-1", 7, true, Vec::new()));
    let now = Instant::now();
    let Some(DaemonCommand::AttachIo {
        id,
        after,
        input,
        cols,
        rows,
    }) = attached.due(now)
    else {
        panic!("the first read is due at once");
    };
    assert_eq!((id.as_str(), after, input.as_str()), ("at-1", 7, ""));
    assert_eq!((cols, rows), (None, None));
    // While it is unanswered, keys wait.
    attached.type_key(key(KeyCode::Char('h'), KeyModifiers::NONE));
    attached.type_key(key(KeyCode::Char('i'), KeyModifiers::NONE));
    assert!(attached.due(now).is_none());
    attached.apply(page("at-1", 8, false, Vec::new()));
    let Some(DaemonCommand::AttachIo { input, after, .. }) = attached.due(now) else {
        panic!("typed keys go at once");
    };
    assert_eq!((input.as_str(), after), ("hi", 8));
    attached.apply(page("at-1", 8, false, Vec::new()));
    // Nothing typed: the next read waits for its interval.
    assert!(attached.due(now).is_none());
    assert!(attached.due(now + READ_EVERY).is_some());
    attached.unanswered();
    // A new size goes once.
    attached.drawn.set((90, 20));
    let Some(DaemonCommand::AttachIo { cols, rows, .. }) = attached.due(now) else {
        panic!("a resize goes at once");
    };
    assert_eq!((cols, rows), (Some(90), Some(20)));
    attached.unanswered();
    assert!(attached.due(now).is_none());
}

#[test]
fn a_long_paste_goes_in_bounded_pieces_on_character_boundaries() {
    let mut attached = Attached::new(page("at-1", 1, true, Vec::new()));
    attached.apply(page("at-1", 1, false, Vec::new()));
    attached.paste(&"é".repeat(MAX_SENT));
    let mut sent = String::new();
    while let Some(DaemonCommand::AttachIo { input, .. }) = attached.due(Instant::now()) {
        assert!(input.len() <= MAX_SENT);
        if input.is_empty() {
            break;
        }
        sent.push_str(&input);
        attached.unanswered();
    }
    assert_eq!(sent, "é".repeat(MAX_SENT));
}
