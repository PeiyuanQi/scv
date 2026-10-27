//! Unit tests for `src/delegate/takeover.rs`.

use std::{
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use super::*;
use crate::delegate::{
    background::lane::Lanes,
    conversation::{ConversationLimits, ConversationStore},
    records::DelegationRegistry,
};

const VENDOR: &str = "0199a213-81c0-7800-8aa1-bbab2a035a53";

/// A fake interactive agent CLI: it prints its arguments, then answers each
/// line typed into its terminal.
const FAKE_AGENT: &str = r#"#!/bin/sh
echo "interactive $*"
while IFS= read -r line; do
  case "$line" in
    bye) echo "leaving"; exit 0 ;;
    secret) echo "export OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz and done" ;;
    *) echo "got $line" ;;
  esac
done
"#;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A session's conversations with one idle Codex conversation in `cwd`,
/// and its lanes.
fn conversation(cwd: &Path) -> (Arc<ConversationStore>, Arc<Lanes>, String) {
    let store = Arc::new(ConversationStore::new(
        ConversationLimits {
            max: 4,
            idle: Duration::from_secs(3600),
        },
        None,
    ));
    let handle = store.adopt("codex", cwd, VENDOR).unwrap();
    (store, Arc::new(Lanes::default()), handle)
}

/// Take the conversation over in `agent`'s terminal.
async fn take_over(
    agent: &Path,
    cwd: &Path,
    delegation: Option<&DelegationContext>,
) -> (Arc<Takeover>, Arc<ConversationStore>, Arc<Lanes>, String) {
    let (store, lanes, handle) = conversation(cwd);
    let launch = launch(
        "codex",
        &agent.display().to_string(),
        &[],
        true,
        &[],
        delegation,
    )
    .unwrap();
    let hold = store.take_over(&handle).unwrap();
    let place = lanes.join(&handle);
    let takeover = start(&launch, hold, place, "session-1", (80, 24))
        .await
        .unwrap();
    (takeover, store, lanes, handle)
}

/// The text of a page's rows, one line per row.
fn text(page: &AttachPage) -> String {
    page.lines
        .iter()
        .map(|row| {
            row.spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read pages until the screen shows `wanted`; returns the whole screen.
async fn screen_shows(takeover: &Takeover, wanted: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let shown = text(&takeover.page("client", 0));
            if shown.contains(wanted) {
                return shown;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the screen never showed {wanted:?}:\n{}",
            text(&takeover.page("client", 0))
        )
    })
}

/// Whether process group `pid` still has a member that is not a zombie.
fn group_alive(pid: u32) -> bool {
    group_exists(pid)
}

/// Whether process `pid` runs: it exists and, where SCV can tell, is not
/// a zombie.
fn alive(pid: i32) -> bool {
    if cfg!(target_os = "linux") {
        return std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        });
    }
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn only_an_agent_whose_terminal_never_asks_is_offered() {
    let dir = tempfile::tempdir().unwrap();
    let codex = script(dir.path(), "codex", FAKE_AGENT);
    let codex = codex.display().to_string();
    let offered = launch("codex", &codex, &[], true, &[], None).unwrap();
    assert_eq!(
        offered.args,
        [
            "resume",
            "{session}",
            "--dangerously-bypass-approvals-and-sandbox",
            "-c",
            "web_search=\"live\""
        ]
    );
    // With its own prompts on, its terminal would ask what SCV's gate decides.
    let asks = launch("codex", &codex, &[], false, &[], None).unwrap_err();
    assert!(asks.contains("permissions = \"full\""), "{asks}");
    assert!(asks.contains("approval gate"), "{asks}");
    for claude in ["claude", "claude-ai"] {
        let refused = launch(claude, &codex, &[], true, &[], None).unwrap_err();
        assert!(refused.contains("approval gate"), "{refused}");
        assert!(refused.contains("deleting files"), "{refused}");
    }
    let unverified = launch("grok", &codex, &[], true, &[], None).unwrap_err();
    assert!(unverified.contains("no interactive mode"), "{unverified}");
    let missing = launch("codex", "/nonexistent/codex", &[], true, &[], None).unwrap_err();
    assert!(missing.contains("was not found"), "{missing}");
}

#[test]
fn sizes_are_kept_within_bounds() {
    assert_eq!(clamp_size(None, None), DEFAULT_SIZE);
    assert_eq!(clamp_size(Some(1), Some(1)), MIN_SIZE);
    assert_eq!(clamp_size(Some(u16::MAX), Some(u16::MAX)), MAX_SIZE);
    assert_eq!(clamp_size(Some(120), Some(40)), (120, 40));
}

fn screen(cols: u16, rows: u16) -> Screen {
    Screen::new(cols, rows)
}

fn rows_text(rows: &[AttachRow]) -> String {
    rows.iter()
        .map(|row| {
            row.spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn terminal_queries_are_answered_as_of_the_output_before_them() {
    let mut screen = screen(80, 24);
    // Cursor to row 4, column 6, then where is it? The query is split
    // across two reads.
    assert_eq!(screen.process(b"\x1b[4;6Hab\x1b["), b"");
    assert_eq!(screen.process(b"6nxyz"), b"\x1b[4;8R");
    assert_eq!(screen.process(b"\x1b[c"), DEVICE_ATTRIBUTES);
    assert_eq!(
        screen.process(b"\x1b[0c\x1b[5n"),
        [DEVICE_ATTRIBUTES, b"\x1b[0n"].concat()
    );
    // Other sequences are not queries.
    assert_eq!(screen.process(b"\x1b[2J\x1b[?25l\x1b[>c\x1b[?6n"), b"");
}

#[test]
fn only_changed_rows_are_sent_until_a_resize() {
    let mut screen = screen(40, 6);
    screen.process(b"first\r\nsecond");
    screen.refresh();
    let all = screen.rows_since(None);
    assert_eq!(all.len(), 6);
    assert_eq!(rows_text(&all[..2]), "first\nsecond");
    let seen = screen.generation;
    screen.process(b"\r\nthird");
    screen.refresh();
    let changed = screen.rows_since(Some(seen));
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(changed[0].row, 2);
    assert_eq!(rows_text(&changed), "third");
    // Nothing new: nothing sent.
    screen.dirty = true;
    screen.refresh();
    assert_eq!(screen.rows_since(Some(screen.generation)).len(), 0);
}

#[test]
fn credentials_on_the_screen_are_masked_by_the_activity_log_rules() {
    let mut screen = screen(60, 8);
    screen.process(
        b"$ export OPENAI_API_KEY=sk-proj-abcdefghijklmnop\r\n\
          Authorization: Bearer abcdef123456\r\n\
          see https://example.com/a?token=hunter2#frag ok\r\n\
          \x1b[31mghp_0123456789abcdef\x1b[0m done\r\n\
          password:      correct-horse\r\n",
    );
    screen.refresh();
    let shown = rows_text(&screen.rows_since(None));
    for secret in [
        "sk-proj",
        "abcdef123456",
        "hunter2",
        "frag",
        "ghp_0123",
        "correct-horse",
    ] {
        assert!(!shown.contains(secret), "{secret} shown:\n{shown}");
    }
    assert!(shown.contains("OPENAI_API_KEY=…"), "{shown}");
    assert!(shown.contains("Bearer …"), "{shown}");
    assert!(shown.contains("https://example.com/a…"), "{shown}");
    assert!(shown.contains("…") && shown.contains(" done"), "{shown}");
    // Columns stay where they were: the mask takes the secret's cells.
    let line = shown.lines().next().unwrap();
    assert_eq!(
        line.chars().count(),
        "$ export OPENAI_API_KEY=…".chars().count()
    );
}

#[test]
fn a_credential_split_across_rows_is_masked_on_both() {
    let mut screen = screen(30, 6);
    // The terminal wraps it; then a UI that wraps lines itself, filling a
    // row to its last column and going on at the next row's start.
    screen.process(b"key: sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789\r\n");
    screen.process(b"0123456789012345678901 ghp_abc\r\ndefghijklmn rest");
    screen.refresh();
    let shown = rows_text(&screen.rows_since(None));
    assert!(shown.contains(" rest"), "{shown}");
    for part in ["sk-ant", "uvwxyz0123456789", "ghp_", "defghijklmn"] {
        assert!(!shown.contains(part), "{part} shown:\n{shown}");
    }
}

#[test]
fn styles_become_spans_and_trailing_blanks_are_left_out() {
    let mut screen = screen(40, 3);
    screen.process(b"\x1b[1;32mok\x1b[0m plain \x1b[48;5;21mblue\x1b[0m   ");
    screen.refresh();
    let rows = screen.rows_since(None);
    let spans = &rows[0].spans;
    assert_eq!(spans[0].text, "ok");
    assert!(spans[0].bold);
    assert_eq!(spans[0].fg, Some(AttachColor::Index(2)));
    assert_eq!(spans[1].text, " plain ");
    assert_eq!(spans[2].text, "blue");
    assert_eq!(spans[2].bg, Some(AttachColor::Index(21)));
    assert_eq!(spans.len(), 3, "{spans:?}");
    assert_eq!(rows[1].spans.len(), 0);
}

#[tokio::test]
async fn typed_keys_reach_the_agent_on_its_own_session_and_its_screen_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let agent = script(dir.path(), "codex", FAKE_AGENT);
    let (takeover, store, lanes, handle) = take_over(&agent, dir.path(), None).await;
    // The line runs over two rows of the 80-column screen.
    let shown = screen_shows(&takeover, "interactive")
        .await
        .replace('\n', "");
    assert!(
        shown.contains(&format!(
            "interactive resume {VENDOR} --dangerously-bypass-approvals-and-sandbox"
        )),
        "{shown}"
    );
    // The conversation runs no turn of SCV's meanwhile, and calls queue.
    assert!(store.is_busy(&handle));
    let begin = store
        .begin("codex", Some(&handle), dir.path(), false)
        .unwrap_err();
    assert!(begin.message.contains("taken over"), "{}", begin.message);
    assert_eq!(lanes.join(&handle).ahead, 1);
    assert!(takeover.input(b"hello\r").await.is_none());
    screen_shows(&takeover, "got hello").await;
    // Only what changed comes after the first page.
    let first = takeover.page("client", 0);
    assert!(first.full);
    takeover.input(b"again\r").await;
    screen_shows(&takeover, "got again").await;
    let next = takeover.page("client", first.generation);
    assert!(!next.full);
    assert!(next.lines.len() < usize::from(next.rows), "{next:?}");
    takeover.end().await;
}

#[tokio::test]
async fn ending_hangs_the_terminal_up_and_gives_the_conversation_back() {
    let dir = tempfile::tempdir().unwrap();
    let agent = script(dir.path(), "codex", FAKE_AGENT);
    let (takeover, store, lanes, handle) = take_over(&agent, dir.path(), None).await;
    screen_shows(&takeover, "interactive").await;
    let pid = takeover.pid;
    takeover.end().await;
    assert!(takeover.is_finished());
    assert!(!group_alive(pid));
    let page = takeover.page("client", 0);
    assert!(page.ended);
    assert_eq!(page.exit.as_deref(), Some("ended from SCV"));
    assert!(!store.is_busy(&handle));
    assert_eq!(lanes.join(&handle).ahead, 0);
    // Its next turn continues the same agent session.
    let turn = store
        .begin("codex", Some(&handle), dir.path(), false)
        .unwrap();
    assert_eq!(turn.vendor.as_deref(), Some(VENDOR));
    assert!(takeover.input(b"late\r").await.is_some());
}

#[tokio::test]
async fn an_agent_that_ignores_the_hangup_is_stopped_with_what_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("child-pid");
    let agent = script(
        dir.path(),
        "codex",
        &format!(
            "#!/bin/sh\ntrap '' HUP TERM\nsleep 300 &\necho $! > {}\necho stubborn\nwait\n",
            marker.display()
        ),
    );
    let (takeover, store, _lanes, handle) = take_over(&agent, dir.path(), None).await;
    screen_shows(&takeover, "stubborn").await;
    let child: i32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let started = std::time::Instant::now();
    takeover.end().await;
    assert!(started.elapsed() < Duration::from_secs(9));
    assert!(takeover.is_finished());
    assert!(!group_alive(takeover.pid));
    assert!(!alive(child), "the agent's own child {child} outlived it");
    assert!(!store.is_busy(&handle));
}

#[tokio::test]
async fn an_agent_that_exits_ends_the_takeover_by_itself() {
    let dir = tempfile::tempdir().unwrap();
    let agent = script(dir.path(), "codex", FAKE_AGENT);
    let (takeover, store, lanes, handle) = take_over(&agent, dir.path(), None).await;
    screen_shows(&takeover, "interactive").await;
    let queued = lanes.join(&handle);
    assert_eq!(queued.ahead, 1);
    takeover.input(b"bye\r").await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !takeover.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let page = takeover.page("client", 0);
    assert!(page.ended);
    assert_eq!(page.exit.as_deref(), Some("exit 0"));
    // The screen keeps what it last showed, for the client still attached.
    assert!(text(&page).contains("leaving"), "{}", text(&page));
    assert!(!store.is_busy(&handle));
    // The call queued behind the takeover is first now.
    tokio::time::timeout(
        Duration::from_secs(1),
        queued.wait_first(&tokio_util::sync::CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
}

#[tokio::test]
async fn a_recorded_takeover_logs_its_life_but_never_its_screen() {
    let home = tempfile::tempdir().unwrap();
    let registry = Arc::new(DelegationRegistry::for_test(&scv_client::Layout::new(
        home.path(),
    )));
    let delegation = DelegationContext {
        registry: Arc::clone(&registry),
        session: "session-1".into(),
        depth: 0,
    };
    let agent = script(home.path(), "codex", FAKE_AGENT);
    let (takeover, _store, _lanes, _handle) =
        take_over(&agent, home.path(), Some(&delegation)).await;
    let run = takeover.run.clone().unwrap();
    assert!(
        registry
            .list(false)
            .iter()
            .any(|entry| entry.record.handle == run)
    );
    screen_shows(&takeover, "interactive").await;
    takeover.input(b"secret\r").await;
    let shown = screen_shows(&takeover, "and done").await;
    assert!(shown.contains("OPENAI_API_KEY=…"), "{shown}");
    assert!(!shown.contains("sk-proj"), "{shown}");
    takeover.note_attached(false);
    takeover.end().await;
    // The record is gone with the process.
    assert!(
        !registry
            .list(true)
            .iter()
            .any(|entry| entry.record.handle == run)
    );
    let state = crate::delegate::activity::load_state(registry.runs_dir(), &run).unwrap();
    assert_eq!(state.transport, "terminal");
    assert!(state.ended);
    assert_eq!(state.state, scv_protocol::AgentState::Completed);
    assert_eq!(state.vendor.as_deref(), Some(VENDOR));
    let log = std::fs::read_to_string(registry.runs_dir().join(format!("{run}.jsonl"))).unwrap();
    assert!(log.contains("taken over by the owner"), "{log}");
    assert!(log.contains("the owner detached"), "{log}");
    assert!(log.contains("the terminal closed: ended from SCV"), "{log}");
    // Nothing the terminal showed is anywhere under the instance home.
    for entry in walk(home.path()) {
        if entry.file_name().is_some_and(|name| name == "codex") {
            continue;
        }
        let bytes = std::fs::read(&entry).unwrap_or_default();
        let content = String::from_utf8_lossy(&bytes);
        for leaked in ["sk-proj", "got hello", "and done", "interactive resume"] {
            assert!(!content.contains(leaked), "{leaked} in {}", entry.display());
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[test]
fn counts_are_bounded_by_the_screen_whatever_carries_them() {
    let scan = |bytes: &[u8]| Scanner::default().scan(bytes, (24, 80)).0;
    assert_eq!(scan(b"\x1b[65535L"), b"\x1b[24L");
    assert_eq!(scan(b"\x1b[65535;9@"), b"\x1b[80;9@");
    assert_eq!(scan(b"\x1b[2M\x1b[999X"), b"\x1b[2M\x1b[80X");
    // A control inside goes first, as the parser carries it out first;
    // bytes the parser ignores inside do not split the number.
    assert_eq!(scan(b"\x1b[655\r35S"), b"\r\x1b[24S");
    assert_eq!(scan(b"\x1b[655\x7f\xc335T"), b"\x1b[24T");
    assert_eq!(scan(b"\x1b[65535:1P"), b"\x1b[80:1P");
    // A control between `ESC` and `[` does not hide the sequence.
    assert_eq!(scan(b"\x1b\r[65535L"), b"\r\x1b[24L");
    // Across reads, and other sequences as they came.
    let mut scanner = Scanner::default();
    assert_eq!(scanner.scan(b"ab\x1b[655", (24, 80)).0, b"ab");
    assert_eq!(scanner.scan(b"35Lcd", (24, 80)).0, b"\x1b[24Lcd");
    assert_eq!(
        scan(b"\x1b[38;2;255;0;0m\x1b[?2004h\x1b]0;t\x07\x1b(B"),
        b"\x1b[38;2;255;0;0m\x1b[?2004h\x1b]0;t\x07\x1b(B"
    );
    // One too long to be real is dropped, its controls kept.
    let long = [b"\x1b[".as_slice(), &[b'0'; 300], b"\r65535L", b"x"].concat();
    assert_eq!(scan(&long), b"\rx");
    // An `ESC` that cuts a UTF-8 character short still starts a sequence:
    // the parser drops the NUL before it instead.
    assert_eq!(scan(b"\xe2\x1b[65535L"), b"\xe2\x00\x1b[24L");
}

#[test]
fn output_whose_cost_grows_with_its_counts_stays_cheap() {
    let mut screen = screen(400, 200);
    screen.process(b"text on the first row");
    let started = std::time::Instant::now();
    for sequence in [
        b"\x1b[65535L".as_slice(),
        b"\x1b[65535@",
        b"\x1b[65535P",
        b"\x1b[65535M",
        b"\x1b[65535S",
        b"\x1b[65535T",
        b"\x1b[65535X",
    ] {
        screen.process(&sequence.repeat(100));
    }
    // Unbounded, the first alone takes seconds in a release build.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // And each did what it does at its full count: the screen is blank.
    screen.refresh();
    assert_eq!(rows_text(&screen.rows_since(None)).trim(), "");
}

#[test]
fn a_cursor_saved_before_the_screen_shrank_is_restored_onto_it() {
    for (save, restore) in [
        (b"\x1b7".as_slice(), b"\x1b8".as_slice()),
        (b"\x1b[?1049h", b"\x1b[?1049l"),
        (b"\x1b[?1048h", b"\x1b[?25;1048l"),
    ] {
        let mut screen = screen(80, 40);
        screen.process(b"\x1b[40;80H");
        screen.process(save);
        screen.parser.set_size(25, 60);
        screen.process(&[restore, b"x"].concat());
        let (row, col) = screen.parser.screen().cursor_position();
        assert!(row < 25 && col <= 60, "{row},{col} after {restore:?}");
        screen.refresh();
        assert!(rows_text(&screen.rows_since(None)).contains('x'));
    }
    // vt100 0.15 alone fails there; a later release clamps it itself, and
    // then this guard can go.
    let mut parser = vt100::Parser::new(40, 80, 0);
    parser.process(b"\x1b[40;80H\x1b7");
    parser.set_size(25, 60);
    let failed = std::panic::catch_unwind(AssertUnwindSafe(|| parser.process(b"\x1b8x")));
    assert!(
        failed.is_err(),
        "vt100 restores the saved cursor onto the screen now"
    );
}

#[test]
fn random_output_and_resizes_never_fail_the_screen() {
    let mut seed: u64 = 0x5eed_cafe_f00d;
    let mut next = move |below: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % below
    };
    const PIECES: &[&[u8]] = &[
        b"\x1b7",
        b"\x1b8",
        b"\x1bD",
        b"\x1bM",
        b"\x1bc",
        b"\r",
        b"\n",
        b"\x08",
        b"\t",
        "宽字".as_bytes(),
        "e\u{301}".as_bytes(),
        b"\xe2",
        b"abcdefghij",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[?47h",
        b"\x1b[?6h",
        b"\x1b[?6l",
        b"\x1b[?7l",
        b"\x1b[?25l",
        b"\x1b[?2004h",
        b"\x1b(0",
        b"\x1b]8;;https://x?token=y\x07",
        b"\x1b#8",
        b"\x7f",
    ];
    const FINALS: &[u8] = b"@ABCDEFGHJKLMPSTXZ`abdefghlmnrsu";
    for _ in 0..16 {
        let (mut rows, mut cols) = (5 + next(196) as u16, 20 + next(381) as u16);
        let mut screen = screen(cols, rows);
        for _ in 0..150 {
            let mut output = Vec::new();
            for _ in 0..=next(6) {
                if next(3) == 0 {
                    output.extend_from_slice(PIECES[next(PIECES.len() as u64) as usize]);
                    continue;
                }
                output.extend_from_slice(b"\x1b[");
                if next(4) == 0 {
                    output.push(b'?');
                }
                for param in 0..next(3) {
                    if param > 0 {
                        output.push(b';');
                    }
                    let value = [next(3), next(30), next(300), next(70_000)][next(4) as usize];
                    output.extend_from_slice(value.to_string().as_bytes());
                }
                output.push(FINALS[next(FINALS.len() as u64) as usize]);
            }
            screen.process(&output);
            if next(20) == 0 {
                (rows, cols) = (5 + next(196) as u16, 20 + next(381) as u16);
                screen.parser.set_size(rows, cols);
            }
            if next(40) == 0 {
                screen.refresh();
                screen.rows_since(None);
            }
        }
    }
}

#[test]
fn a_value_a_ui_wraps_onto_an_indented_row_is_masked_on_both() {
    let mut screen = screen(30, 6);
    // Row 0 ends at the right edge inside a key; the UI goes on with it
    // on an indented row, which the terminal does not see as a wrap.
    screen.process(b"key sk-proj-abcdefghijklmnopqr\r\n");
    screen.process(b"   stuvwxyz0123456789 shown\r\n");
    // A name at a row's end whose value the UI put on the next row.
    screen.process(b"Authorization: Bearer\r\n");
    screen.process(b"  \xe2\x94\x82 tok-0123456789 also\r\n");
    screen.process(b"$ ls\r\n");
    screen.refresh();
    let shown = rows_text(&screen.rows_since(None));
    for part in ["sk-proj", "stuvwxyz", "tok-0123"] {
        assert!(!shown.contains(part), "{part} shown:\n{shown}");
    }
    for kept in ["shown", "also", "$ ls"] {
        assert!(shown.contains(kept), "{kept} hidden:\n{shown}");
    }
}

#[test]
fn a_row_whose_mask_changes_is_sent_again() {
    let mut screen = screen(30, 4);
    screen.process(b"\x1b[2;1Hsecret-word-here\x1b[1;1H");
    screen.refresh();
    let seen = screen.generation;
    // The row above now ends in a name waiting for its value.
    screen.process(b"password:");
    screen.refresh();
    let changed = screen.rows_since(Some(seen));
    assert!(changed.iter().any(|row| row.row == 1), "{changed:?}");
    assert!(!rows_text(&changed).contains("secret-word"), "{changed:?}");
}

#[tokio::test]
async fn a_screen_that_fails_ends_the_takeover_and_gives_the_conversation_back() {
    let dir = tempfile::tempdir().unwrap();
    let agent = script(
        dir.path(),
        "codex",
        &format!(
            "#!/bin/sh\necho ready\nwhile IFS= read -r line; do printf '{}'; done\n",
            String::from_utf8_lossy(FAIL_FOR_TESTS)
                .replace('\x1b', "\\033")
                .replace('\x07', "\\007")
        ),
    );
    let (takeover, store, _lanes, handle) = take_over(&agent, dir.path(), None).await;
    screen_shows(&takeover, "ready").await;
    takeover.input(b"boom\r").await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !takeover.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a takeover whose screen failed went on");
    let page = takeover.page("client", 0);
    assert_eq!(
        page.exit.as_deref(),
        Some("SCV could not keep its screen, so it was ended")
    );
    assert!(text(&page).contains("ready"), "{}", text(&page));
    assert!(!store.is_busy(&handle));
    // Typing into it now is refused at once.
    let note = takeover.input(b"more\r").await;
    assert!(note.is_some_and(|note| note.contains("closed")));
}
