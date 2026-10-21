//! Unit tests for `src/delegate/activity.rs`.

use super::*;

fn header(handle: &str) -> RunHeader {
    RunHeader {
        handle: handle.into(),
        agent: "codex".into(),
        session: "session-1".into(),
        transport: RunTransport::Acp,
        cwd: PathBuf::from("/w/project"),
        conversation: Some(("codex-1".into(), 1)),
        streams: true,
    }
}

/// The runs directory inside a fresh instance's private state directory,
/// as `Layout::runs` places it.
struct Runs(tempfile::TempDir);

impl Runs {
    fn path(&self) -> PathBuf {
        self.0.path().join("runs")
    }
}

fn runs() -> Runs {
    Runs(tempfile::tempdir().unwrap())
}

fn open(dir: &Path, handle: &str) -> RunActivity {
    RunActivity::open(dir, ProcessIdentity::current().unwrap(), header(handle)).unwrap()
}

fn events(dir: &Path, handle: &str) -> Vec<RunEvent> {
    read_page(dir, handle, 0, MAX_PAGE_EVENTS).unwrap().events
}

#[test]
fn a_run_logs_its_turns_message_lines_tools_and_end() {
    let dir = runs();
    let activity = open(&dir.path(), "codex-a1b2c3");
    let state = load_state(&dir.path(), "codex-a1b2c3").unwrap();
    assert_eq!(state.state, AgentState::Starting);
    assert_eq!(state.conversation.as_deref(), Some("codex-1"));
    assert_eq!(state.transport, "acp");

    activity.turn_started(Some("codex-1"), 2, "Fix the flaky test", Some("job-4"));
    let state = load_state(&dir.path(), "codex-a1b2c3").unwrap();
    assert_eq!(
        (state.state, state.turn, state.job.as_deref()),
        (AgentState::Working, Some(2), Some("job-4"))
    );
    // Deltas become whole lines; a tool line ends the pending one.
    activity.text("Looking at the ");
    activity.text("test.\nIt sleeps");
    activity.tool("$ cargo test -p shop");
    activity.tool("Edit …/src/cart.rs failed");
    activity.plan("make the lock explicit");
    activity.message("Done.\nAll green.");
    activity.turn_ended("completed", None);
    activity.idle();
    activity.run_ended("exited");

    let kinds: Vec<(RunEventKind, String)> = events(&dir.path(), "codex-a1b2c3")
        .into_iter()
        .map(|event| (event.kind, event.text))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (
                RunEventKind::RunStarted,
                "codex over acp in /w/project".into()
            ),
            (RunEventKind::TurnStarted, "Fix the flaky test".into()),
            (RunEventKind::Text, "Looking at the test.".into()),
            (RunEventKind::Text, "It sleeps".into()),
            (RunEventKind::Tool, "$ cargo test -p shop".into()),
            (RunEventKind::ToolFailed, "Edit …/src/cart.rs failed".into()),
            (RunEventKind::Plan, "make the lock explicit".into()),
            (RunEventKind::Text, "Done.".into()),
            (RunEventKind::Text, "All green.".into()),
            (RunEventKind::TurnEnded, String::new()),
            (RunEventKind::RunEnded, String::new()),
        ]
    );
    let state = load_state(&dir.path(), "codex-a1b2c3").unwrap();
    // Between turns it was idle, so its process ending completes it.
    assert!(state.ended);
    assert_eq!(state.state, AgentState::Completed);
    assert_eq!(state.last_line.as_deref(), Some("All green."));
}

#[test]
fn approvals_mark_the_run_waiting_until_answered() {
    let dir = runs();
    let activity = open(&dir.path(), "claude-0a0b0c");
    activity.turn_started(None, 1, "Deploy", None);
    activity.approval_requested("process", "[claude-1 acp] Run make deploy (execute)");
    let waiting = load_state(&dir.path(), "claude-0a0b0c").unwrap();
    assert_eq!(waiting.state, AgentState::AwaitingInput);
    assert!(waiting.detail.unwrap().contains("make deploy"));
    activity.approval_resolved(false);
    assert_eq!(
        load_state(&dir.path(), "claude-0a0b0c").unwrap().state,
        AgentState::Working
    );
    let resolved = events(&dir.path(), "claude-0a0b0c").pop().unwrap();
    assert_eq!(resolved.kind, RunEventKind::ApprovalResolved);
    assert_eq!(resolved.status.as_deref(), Some("denied"));
}

#[test]
fn message_text_is_redacted_and_control_characters_are_inert() {
    let dir = runs();
    let activity = open(&dir.path(), "codex-000001");
    activity.message("export API_TOKEN=sk-abcdefghijklmnop now\u{1b}[31m red");
    let text = events(&dir.path(), "codex-000001").pop().unwrap().text;
    assert!(!text.contains("sk-abcdefghijklmnop"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
}

#[test]
fn pages_follow_byte_positions_and_resynchronize_mid_line() {
    let dir = runs();
    let activity = open(&dir.path(), "codex-000002");
    for line in 0..10 {
        activity.tool(&format!("$ step {line}"));
    }
    let first = read_page(&dir.path(), "codex-000002", 0, 4).unwrap();
    assert_eq!(first.events.len(), 4);
    assert!(first.more && !first.ended);
    let second = read_page(&dir.path(), "codex-000002", first.next, 100).unwrap();
    assert_eq!(second.events.len(), 7);
    assert_eq!(second.events[0].text, "$ step 3");
    assert!(!second.more);
    // Nothing new: the same position comes back.
    let idle = read_page(&dir.path(), "codex-000002", second.next, 100).unwrap();
    assert_eq!(idle.events, []);
    assert_eq!(idle.next, second.next);
    // A position inside a line skips to the next whole one.
    let skewed = read_page(&dir.path(), "codex-000002", first.next + 3, 100).unwrap();
    assert_eq!(skewed.events[0].text, "$ step 4");
    activity.run_ended("exited");
    let last = read_page(&dir.path(), "codex-000002", second.next, 100).unwrap();
    assert!(last.ended);
    assert!(read_page(&dir.path(), "../etc/passwd", 0, 1).is_err());
}

#[test]
fn a_full_log_keeps_only_turns_approvals_and_the_end() {
    let dir = runs();
    let activity = open(&dir.path(), "codex-000003");
    let line = "x".repeat(1900);
    // Each event is a little over 1900 bytes: this passes the limit.
    for _ in 0..MAX_LOG_BYTES / 1900 + 10 {
        activity.message(&line);
    }
    activity.message("this is dropped");
    activity.turn_ended("failed", Some("out of time"));
    let all: Vec<RunEvent> = {
        let mut events = Vec::new();
        let mut after = 0;
        loop {
            let page = read_page(&dir.path(), "codex-000003", after, MAX_PAGE_EVENTS).unwrap();
            events.extend(page.events);
            after = page.next;
            if !page.more {
                break events;
            }
        }
    };
    let truncated = all
        .iter()
        .position(|event| event.kind == RunEventKind::Truncated)
        .unwrap();
    assert!(
        all[truncated..]
            .iter()
            .all(|event| event.text != "this is dropped")
    );
    assert_eq!(all.last().unwrap().kind, RunEventKind::TurnEnded);
    let state = load_state(&dir.path(), "codex-000003").unwrap();
    assert!(state.bytes < MAX_LOG_BYTES + 8 * 1024);
}

#[test]
fn a_run_whose_process_died_is_settled_as_interrupted() {
    let dir = runs();
    let dead = ProcessIdentity {
        pid: u32::MAX - 1,
        start_time: 1,
    };
    let activity = RunActivity::open(&dir.path(), dead, header("codex-000004")).unwrap();
    activity.turn_started(None, 1, "work", Some("job-1"));
    // The owner crashed: nothing ran its drop.
    std::mem::forget(activity);
    let (state, _) = current(
        &load_state(&dir.path(), "codex-000004").unwrap(),
        unix_now(),
    );
    assert_eq!(state, AgentState::Interrupted);
    assert_eq!(settle_orphans(&dir.path()), vec!["codex-000004".to_owned()]);
    let state = load_state(&dir.path(), "codex-000004").unwrap();
    assert!(state.ended);
    assert_eq!(state.state, AgentState::Interrupted);
    assert_eq!(
        events(&dir.path(), "codex-000004")
            .last()
            .unwrap()
            .status
            .as_deref(),
        Some("lost")
    );
    assert_eq!(settle_orphans(&dir.path()), [] as [String; 0]);
}

#[test]
fn a_quiet_working_run_reads_as_unknown_unless_it_prints_only_at_the_end() {
    let dir = runs();
    let activity = open(&dir.path(), "codex-000005");
    activity.turn_started(None, 1, "work", None);
    activity.tool("$ cargo build");
    let mut state = load_state(&dir.path(), "codex-000005").unwrap();
    let later = state.last_activity_unix + QUIET_AFTER.as_secs() + 60;
    let (quiet, detail) = current(&state, later);
    assert_eq!(quiet, AgentState::Unknown);
    assert!(detail.unwrap().contains("no activity for 6 min"));
    state.streams = false;
    assert_eq!(current(&state, later).0, AgentState::Working);
}

#[test]
fn ended_runs_are_pruned_by_age_and_count_but_running_ones_stay() {
    let dir = runs();
    for handle in ["codex-00000a", "codex-00000b", "codex-00000c"] {
        open(&dir.path(), handle).run_ended("exited");
    }
    let running = open(&dir.path(), "codex-00000d");
    running.turn_started(None, 1, "keep me", None);
    assert_eq!(prune(&dir.path(), Duration::from_secs(3600), 2), 1);
    assert_eq!(list_states(&dir.path()).len(), 3);
    assert_eq!(
        prune(&dir.path(), Duration::ZERO, 10),
        0,
        "nothing is older than a second yet"
    );
    assert!(load_state(&dir.path(), "codex-00000d").is_some());
}

#[test]
fn only_delegation_handles_name_logs() {
    assert!(valid_handle("codex-3f9a2c"));
    assert!(valid_handle("claude-ai-3f9a2c"));
    for bad in [
        "codex",
        "codex-3F9A2C",
        "../x-000000",
        "codex-3f9a2",
        "-000000",
        "Codex-000000",
    ] {
        assert!(!valid_handle(bad), "{bad}");
    }
}

#[test]
fn relayed_tool_lines_and_approval_summaries_are_scrubbed() {
    // A nested SCV relays its own tools' summaries and lines, which SCV
    // did not shorten: the log scrubs them all the same.
    let dir = runs();
    let activity = open(&dir.path(), "scv-000010");
    activity.turn_started(None, 1, "deploy", None);
    activity.approval_requested(
        "process",
        "Run with /bin/bash -lc: curl -H \"Authorization: Bearer ghp_abcdefghijklmnopqrstuvwx\"\nhttps://api.example.com/v1?token=hunter2",
    );
    activity.tool("$ export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwx && fetch https://x.example/a?sig=s3cret#frag");
    activity.plan("call https://x.example/b?key=k3y");
    let logged: String = events(&dir.path(), "scv-000010")
        .iter()
        .map(|event| event.text.clone())
        .collect();
    let state = load_state(&dir.path(), "scv-000010").unwrap();
    let kept = format!("{logged}{:?}{:?}", state.detail, state.last_line);
    for secret in ["ghp_abcdefghijklmnopqrstuvwx", "hunter2", "s3cret", "k3y"] {
        assert!(!kept.contains(secret), "{secret} in {kept}");
    }
    assert!(logged.contains("https://api.example.com/v1"), "{logged}");
}

#[test]
fn a_run_whose_process_ends_mid_turn_logs_the_turns_end_first() {
    let dir = runs();
    let activity = open(&dir.path(), "codex-000011");
    activity.turn_started(Some("codex-1"), 1, "work", Some("job-1"));
    // The reaper sees the process go before the call reports why.
    activity.run_ended("exited");
    let page = read_page(&dir.path(), "codex-000011", 0, 100).unwrap();
    assert!(!page.ended, "a reader keeps following");
    assert_eq!(
        load_state(&dir.path(), "codex-000011").unwrap().state,
        AgentState::Failed
    );
    // An answer arriving now changes nothing.
    activity.approval_resolved(true);
    activity.turn_ended("failed", Some("connection lost: segfault"));
    let kinds: Vec<RunEventKind> = events(&dir.path(), "codex-000011")
        .iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        &kinds[kinds.len() - 2..],
        [RunEventKind::TurnEnded, RunEventKind::RunEnded]
    );
    let page = read_page(&dir.path(), "codex-000011", page.next, 100).unwrap();
    assert!(page.ended);
    let state = load_state(&dir.path(), "codex-000011").unwrap();
    assert!(state.ended && state.ended_unix.is_some());
    assert_eq!(state.state, AgentState::Failed);

    // A call dropped before it logged the turn's end ends the run as it
    // lets go of the turn.
    let dropped = open(&dir.path(), "codex-000012");
    dropped.turn_started(None, 1, "work", None);
    dropped.run_ended("killed");
    assert!(!load_state(&dir.path(), "codex-000012").unwrap().ended);
    dropped.turn_released();
    assert!(load_state(&dir.path(), "codex-000012").unwrap().ended);
}

#[test]
fn a_run_ending_between_turns_ends_as_its_last_turn_did() {
    let dir = runs();
    let activity = open(&dir.path(), "codex-000013");
    activity.turn_started(Some("codex-1"), 1, "work", Some("job-1"));
    activity.turn_ended("timeout", Some("timed out after 60 seconds"));
    activity.idle();
    activity.turn_started(Some("codex-1"), 2, "more", Some("job-2"));
    activity.turn_ended("timeout", Some("timed out after 60 seconds"));
    activity.idle();
    activity.run_ended("exited");
    let state = load_state(&dir.path(), "codex-000013").unwrap();
    assert!(state.ended);
    assert_eq!(state.state, AgentState::Timeout);
    // Both jobs whose turns it served still find it.
    assert_eq!(state.jobs, ["job-1", "job-2"]);
    assert_eq!(state.job.as_deref(), Some("job-2"));
}

#[test]
fn an_interrupted_run_is_kept_for_the_full_time_after_it_is_settled() {
    let dir = runs();
    let dead = ProcessIdentity {
        pid: u32::MAX - 1,
        start_time: 1,
    };
    let activity = RunActivity::open(&dir.path(), dead, header("codex-000014")).unwrap();
    activity.turn_started(None, 1, "work", Some("job-1"));
    std::mem::forget(activity);
    // Its last activity was long ago: the host was down for weeks.
    let mut state = load_state(&dir.path(), "codex-000014").unwrap();
    state.last_activity_unix = 1;
    write_private_json(&dir.path(), "codex-000014.state.json", &state).unwrap();
    settle_orphans(&dir.path());
    assert_eq!(prune(&dir.path(), Duration::from_secs(3600), 10), 0);
    assert!(load_state(&dir.path(), "codex-000014").is_some());
}
