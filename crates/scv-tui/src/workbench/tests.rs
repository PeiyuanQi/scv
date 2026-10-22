//! Unit tests for `src/workbench.rs`.

use scv_protocol::{DaemonStatus, DelegationSummary, WorktreeRef};

use super::*;

fn job(session: &str, name: &str, state: AgentState) -> WorkbenchJob {
    WorkbenchJob {
        session: session.into(),
        job: name.into(),
        agent: "codex".into(),
        task: format!("task of {name}"),
        workspace: "/w/projects".into(),
        cwd: "/w/projects/shop".into(),
        state,
        detail: None,
        started_by: JobOrigin::Model,
        conversation: Some("codex-1".into()),
        run: None,
        worktree: None,
        reviewed: false,
        resumable: state == AgentState::Interrupted,
        resumed_as: None,
        created_unix_seconds: 100,
        updated_unix_seconds: 100,
    }
}

fn run(handle: &str, session: &str, job: Option<&str>, started: u64) -> WorkbenchRun {
    WorkbenchRun {
        handle: handle.into(),
        agent: "codex".into(),
        session: session.into(),
        transport: "cli".into(),
        conversation: Some("codex-1".into()),
        turn: Some(1),
        job: job.map(str::to_owned),
        jobs: job.into_iter().map(str::to_owned).collect(),
        cwd: "/w/projects/shop".into(),
        state: AgentState::Working,
        detail: None,
        started_unix_seconds: started,
        last_activity_unix_seconds: started,
        live: true,
        steerable: false,
    }
}

fn approval(id: &str, job: &str) -> WorkbenchApproval {
    WorkbenchApproval {
        id: id.into(),
        session: "s".into(),
        job: job.into(),
        name: "agent".into(),
        risk: "process".into(),
        cwd: "/w".into(),
        summary: "Run make deploy".into(),
        since_unix_seconds: 1,
        expires_unix_seconds: 121,
    }
}

fn status(snapshot: WorkbenchSnapshot) -> Box<DaemonStatus> {
    Box::new(DaemonStatus {
        version: "0".into(),
        pid: 1,
        components: Vec::new(),
        delegations: DelegationSummary::default(),
        restart: None,
        confirm: None,
        project: None,
        mail_actions: Vec::new(),
        mail_note: None,
        workbench: Some(snapshot),
        run_log: None,
        workbench_note: None,
        workbench_job: None,
        worktrees: Vec::new(),
    })
}

fn page(handle: &str, texts: &[&str], next: u64, ended: bool) -> Box<DaemonStatus> {
    let mut status = status(WorkbenchSnapshot::default());
    status.workbench = None;
    status.run_log = Some(RunLogPage {
        handle: handle.into(),
        events: texts
            .iter()
            .enumerate()
            .map(|(index, text)| RunEvent {
                seq: index as u64 + 1,
                ts_ms: 0,
                kind: RunEventKind::Text,
                text: (*text).into(),
                turn: None,
                status: None,
                job: None,
                risk: None,
                by: None,
            })
            .collect(),
        next,
        ended,
        more: false,
    });
    status
}

#[test]
fn the_sidebar_groups_approvals_tabs_workspaces_interrupted_jobs_and_queues() {
    let mut workbench = Workbench::default();
    let mut finished: Vec<WorkbenchJob> = (0..12)
        .map(|index| job("s", &format!("job-{}", index + 10), AgentState::Completed))
        .collect();
    let mut jobs = vec![
        job("s", "job-1", AgentState::Working),
        job("s", "job-2", AgentState::Interrupted),
    ];
    jobs[0].worktree = Some(WorktreeRef {
        path: "/h/worktrees/shop-1/codex-a".into(),
        branch: "scv/codex-a".into(),
        repo: "/w/projects/shop".into(),
    });
    jobs.append(&mut finished);
    let note = workbench.answer(Answer::Status(
        Tag::Snapshot,
        status(WorkbenchSnapshot {
            jobs,
            runs: vec![run("codex-00000f", "s", None, 5)],
            approvals: vec![approval("ap-1", "job-1")],
            restorable: vec![RestorableSession {
                id: "old".into(),
                label: "TUI".into(),
                workspace: "/w".into(),
                queued: 2,
                interrupted: None,
                saved_unix_seconds: 1,
            }],
            ..WorkbenchSnapshot::default()
        }),
    ));
    assert_eq!(note, None);
    let items = workbench.items(&[(7, "1 ○ projects".into())]);
    let headers: Vec<String> = items
        .iter()
        .filter_map(|item| match item {
            Item::Header(title) => Some(title.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        headers,
        [
            "Waiting for you (1)",
            "This TUI",
            "/w/projects",
            "Interrupted (1)",
            "Restorable queues"
        ]
    );
    let shown_jobs = items
        .iter()
        .filter(|item| matches!(item, Item::Job(job) if job.state.is_final() && !job.resumable))
        .count();
    assert_eq!(shown_jobs, FINISHED_SHOWN);
    assert!(items.contains(&Item::Tab(7, "1 ○ projects".into())));
    assert!(
        items
            .iter()
            .any(|item| matches!(item, Item::Run(run) if run.handle == "codex-00000f"))
    );
    assert!(!items[0].selectable() && items[1].selectable());
    assert_eq!(workbench.counts(), (1, 0, 1));
}

#[test]
fn changes_between_snapshots_raise_notices_and_ring_for_what_needs_the_owner() {
    let mut workbench = Workbench::default();
    let take = |workbench: &mut Workbench, snapshot| {
        workbench.answer(Answer::Status(Tag::Snapshot, status(snapshot)));
    };
    take(
        &mut workbench,
        WorkbenchSnapshot {
            jobs: vec![
                job("s", "job-1", AgentState::Working),
                job("s", "job-2", AgentState::Working),
                job("s", "job-3", AgentState::Working),
            ],
            ..WorkbenchSnapshot::default()
        },
    );
    assert!(workbench.notices.is_empty() && !workbench.bell);
    let mut failed = job("s", "job-2", AgentState::Failed);
    failed.detail = Some("tests fail".into());
    take(
        &mut workbench,
        WorkbenchSnapshot {
            jobs: vec![
                job("s", "job-1", AgentState::Completed),
                failed,
                job("s", "job-3", AgentState::AwaitingInput),
            ],
            approvals: vec![approval("ap-1", "job-3")],
            ..WorkbenchSnapshot::default()
        },
    );
    let texts: Vec<&str> = workbench
        .notices
        .iter()
        .map(|notice| notice.text.as_str())
        .collect();
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("job-3 asks for approval (process)")),
        "{texts:?}"
    );
    assert!(texts.contains(&"job-1 (codex) completed"), "{texts:?}");
    assert!(
        texts.contains(&"job-2 (codex) failed: tests fail"),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("job-3 (codex) is waiting for input"))
    );
    assert!(workbench.bell);
    assert_eq!(
        workbench.latest_notice(Instant::now()).unwrap().text,
        *texts.last().unwrap()
    );
    assert!(
        workbench
            .latest_notice(Instant::now() + NOTICE_SHOWN + Duration::from_secs(1))
            .is_none()
    );
}

#[test]
fn an_open_view_reads_its_runs_logs_once_each_and_in_order() {
    let mut workbench = Workbench::default();
    let now = Instant::now();
    let first = workbench.due(now);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].tag, Tag::Snapshot);
    // Not again while that one is out.
    assert!(
        workbench
            .due(now + Duration::from_secs(5))
            .iter()
            .all(|request| request.tag != Tag::Snapshot)
    );
    let mut later = run("codex-0000b2", "s", None, 20);
    later.job = None;
    workbench.answer(Answer::Status(
        Tag::Snapshot,
        status(WorkbenchSnapshot {
            jobs: vec![job("s", "job-1", AgentState::Working)],
            runs: vec![run("codex-0000a1", "s", Some("job-1"), 200), later],
            ..WorkbenchSnapshot::default()
        }),
    ));
    let target = Target::Job {
        session: "s".into(),
        job: "job-1".into(),
    };
    workbench.open(target.clone());
    // The job's own run, and later runs of its conversation; not earlier ones.
    let snapshot = workbench.snapshot.clone().unwrap();
    let handles: Vec<&str> = runs_of(&snapshot, &target)
        .iter()
        .map(|run| run.handle.as_str())
        .collect();
    assert_eq!(handles, ["codex-0000a1"]);
    let requests = workbench.due(now + Duration::from_secs(10));
    let logs: Vec<&Request> = requests
        .iter()
        .filter(|request| matches!(request.tag, Tag::Log(..)))
        .collect();
    assert_eq!(logs.len(), 1);
    assert!(matches!(
        &logs[0].command,
        DaemonCommand::RunLog { handle, after: 0, .. } if handle == "codex-0000a1"
    ));
    // A page read from where the view was arrives once: the same page
    // again, as a request repeated after a reconnection brings it, is a
    // page from before the view's position now.
    workbench.answer(Answer::Status(
        Tag::Log("codex-0000a1".into(), 0),
        page("codex-0000a1", &["one", "two"], 40, false),
    ));
    workbench.answer(Answer::Status(
        Tag::Log("codex-0000a1".into(), 0),
        page("codex-0000a1", &["one", "two"], 40, false),
    ));
    let view = &workbench.views[&target];
    assert_eq!(view.lines.len(), 2);
    assert_eq!(view.cursors["codex-0000a1"], (40, false));
    // An ended log is read no more.
    let next = workbench.due(now + Duration::from_secs(20));
    assert!(
        next.iter()
            .any(|request| matches!(&request.command, DaemonCommand::RunLog { after: 40, .. }))
    );
    workbench.answer(Answer::Status(
        Tag::Log("codex-0000a1".into(), 40),
        page("codex-0000a1", &["three"], 60, true),
    ));
    let next = workbench.due(now + Duration::from_secs(30));
    assert!(
        !next
            .iter()
            .any(|request| matches!(request.tag, Tag::Log(..)))
    );
    assert_eq!(
        workbench.conversation(&target),
        Some(("s".into(), "codex-1".into()))
    );
    assert_eq!(workbench.state(&target).0, AgentState::Working);
}

#[test]
fn an_old_daemon_or_a_lost_connection_is_shown_not_retried_in_a_loop() {
    let now = Instant::now();
    let mut workbench = Workbench::default();
    workbench.due(now);
    let note = workbench.answer(Answer::Unsupported).unwrap();
    assert!(note.contains("no workbench"));
    assert_eq!(workbench.availability, Availability::Unsupported);
    assert!(workbench.due(now + Duration::from_secs(2)).is_empty());
    // A daemon restarted into this release is found within half a minute,
    // and an old one is not announced twice.
    assert_eq!(workbench.due(now + Duration::from_secs(31)).len(), 1);
    assert_eq!(workbench.answer(Answer::Unsupported), None);
    workbench.answer(Answer::Status(
        Tag::Snapshot,
        status(WorkbenchSnapshot::default()),
    ));
    assert_eq!(workbench.availability, Availability::Ready);

    let mut workbench = Workbench::default();
    workbench.open(Target::Run {
        handle: "codex-0000a1".into(),
    });
    workbench.due(now);
    workbench.answer(Answer::Disconnected(Tag::Snapshot));
    assert_eq!(workbench.availability, Availability::Disconnected);
    // The next tick asks for the snapshot again, and nothing else until
    // the daemon answers.
    let again = workbench.due(now + Duration::from_secs(2));
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].tag, Tag::Snapshot);
    let refused = workbench.answer(Answer::Refused(Tag::Action, "no such job".into()));
    assert_eq!(refused.as_deref(), Some("no such job"));
    // An action whose connection broke is not silently dropped.
    let lost = workbench.answer(Answer::Disconnected(Tag::Action)).unwrap();
    assert!(lost.contains("check whether"), "{lost}");
}

#[test]
fn a_sent_prompt_comes_back_when_it_is_refused_or_lost() {
    let mut workbench = Workbench::default();
    let target = Target::Run {
        handle: "codex-0000a1".into(),
    };
    workbench.open(target.clone()).sent = Some("also run clippy".into());
    let note = workbench
        .answer(Answer::Refused(
            Tag::Send(target.clone()),
            "conversation codex-1 is unknown".into(),
        ))
        .unwrap();
    assert!(note.contains("unknown"));
    assert_eq!(workbench.views[&target].input, "also run clippy");
    let view = workbench.views.get_mut(&target).unwrap();
    view.input.clear();
    view.sent = Some("again".into());
    workbench.answer(Answer::Disconnected(Tag::Send(target.clone())));
    assert_eq!(workbench.views[&target].input, "again");
    // Typed while the send was in flight: both lines come back, the refused
    // one first.
    let view = workbench.views.get_mut(&target).unwrap();
    view.input = "second".into();
    view.sent = Some("first".into());
    workbench.answer(Answer::Refused(
        Tag::Send(target.clone()),
        "still busy".into(),
    ));
    assert_eq!(workbench.views[&target].input, "first\nsecond");
}

#[test]
fn a_job_seen_first_already_finished_or_waiting_is_news() {
    let mut workbench = Workbench::default();
    workbench.answer(Answer::Status(
        Tag::Snapshot,
        status(WorkbenchSnapshot::default()),
    ));
    workbench.answer(Answer::Status(
        Tag::Snapshot,
        status(WorkbenchSnapshot {
            jobs: vec![
                job("s", "job-1", AgentState::Failed),
                job("s", "job-2", AgentState::Working),
            ],
            ..WorkbenchSnapshot::default()
        }),
    ));
    let texts: Vec<&str> = workbench
        .notices
        .iter()
        .map(|notice| notice.text.as_str())
        .collect();
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(texts[0].starts_with("job-1"), "{texts:?}");
    assert!(workbench.bell);
}

#[test]
fn marks_and_paths_stay_readable() {
    assert_eq!(mark(AgentState::AwaitingInput), "?");
    assert_eq!(mark(AgentState::Interrupted), "⟳");
    assert_eq!(short_path("/a/very/long/path/to/project", 10), "…o/project");
    assert_eq!(short_path("/short", 10), "/short");
}
