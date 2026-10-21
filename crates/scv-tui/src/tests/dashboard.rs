//! The dashboard: tabs, panes, the sidebar, agent views, and the workbench
//! requests keys and commands send.

use scv_protocol::{
    AgentState, DaemonCommand, DaemonStatus, DelegationSummary, JobOrigin, SendMode,
    WorkbenchApproval, WorkbenchJob, WorkbenchSession, WorkbenchSnapshot,
};
use tokio::sync::mpsc;

use super::*;
use crate::{
    poller::{Answer, Request, Tag},
    render::render_ui,
    ui::{ChatTab, Focus, Opening, TabKind, Ui},
    workbench::{Item, Target},
};

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn snapshot() -> WorkbenchSnapshot {
    WorkbenchSnapshot {
        generated_unix_seconds: 1,
        sessions: vec![WorkbenchSession {
            id: "chat-session".into(),
            label: "WeChat".into(),
            workspace: "/w/projects".into(),
            live: true,
            state: AgentState::Working,
            detail: None,
        }],
        jobs: vec![WorkbenchJob {
            session: "chat-session".into(),
            job: "job-3".into(),
            agent: "codex".into(),
            task: "Fix the flaky test".into(),
            workspace: "/w/projects".into(),
            cwd: "/w/projects/shop".into(),
            state: AgentState::AwaitingInput,
            detail: Some("waiting for your approval: Run make deploy".into()),
            started_by: JobOrigin::Model,
            conversation: Some("codex-2".into()),
            run: None,
            worktree: None,
            reviewed: false,
            resumable: false,
            resumed_as: None,
            created_unix_seconds: 1,
            updated_unix_seconds: 1,
        }],
        runs: Vec::new(),
        approvals: vec![WorkbenchApproval {
            id: "ap-1".into(),
            session: "chat-session".into(),
            job: "job-3".into(),
            name: "agent".into(),
            risk: "process".into(),
            cwd: "/w/projects/shop".into(),
            summary: "[codex-2 acp] Run make deploy (execute)".into(),
            since_unix_seconds: 1,
            expires_unix_seconds: 121,
        }],
        restorable: Vec::new(),
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

/// A dashboard with one connected chat tab, whose workbench requests come
/// out of the returned receiver.
async fn dashboard() -> (
    Ui,
    mpsc::UnboundedReceiver<Request>,
    BufReader<UnixStream>,
    SocketPath,
) {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let (client, app, peer) = connected(&path, &listener).await;
    let mut ui = Ui::new(
        ChatTab {
            client,
            app,
            cwd: "/tmp".into(),
        },
        &std::collections::BTreeMap::new(),
        LaunchOptions::default(),
    );
    let (requests, received) = mpsc::unbounded_channel();
    ui.requests = Some(requests);
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(snapshot())));
    (ui, received, peer, path)
}

fn screen(ui: &Ui, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render_ui(frame, ui)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|row| {
            (0..width)
                .map(|column| buffer[(column, row)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_sidebar_opens_a_jobs_live_view_whose_keys_answer_and_send() {
    let (mut ui, mut requests, _peer, _path) = dashboard().await;
    let shown = screen(&ui, 120, 36);
    assert!(shown.contains("Waiting for you (1)"), "{shown}");
    assert!(shown.contains("job-3"), "{shown}");
    assert!(shown.contains("1 waiting for you"), "{shown}");

    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Sidebar);
    let items = ui.sidebar_items();
    let approval = items
        .iter()
        .position(|item| matches!(item, Item::Approval(_)))
        .unwrap();
    ui.workbench.select(&items, approval);
    // y on a waiting approval answers it.
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    let request = requests.try_recv().unwrap();
    assert_eq!(request.tag, Tag::Action);
    assert_eq!(
        request.command,
        DaemonCommand::WorkbenchApprove {
            id: "ap-1".into(),
            approved: true
        }
    );
    // Enter opens the job's view as a tab in the pane.
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(ui.tabs.len(), 2);
    let target = Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    };
    assert!(
        matches!(&ui.tab(ui.active_tab().unwrap()).unwrap().kind, TabKind::Agent(open) if *open == target)
    );
    assert_eq!(ui.focus, Focus::Pane(0));
    let shown = screen(&ui, 120, 36);
    assert!(shown.contains("asks (process)"), "{shown}");
    assert!(shown.contains("send to codex-2"), "{shown}");
    // With the composer empty, n denies.
    ui.key(key(KeyCode::Char('n'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchApprove {
            id: "ap-1".into(),
            approved: false
        }
    );
    // Typing goes to the composer, and Enter sends it to the conversation.
    for character in "go on".chars() {
        ui.key(key(KeyCode::Char(character), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchSend {
            session: "chat-session".into(),
            conversation: "codex-2".into(),
            prompt: "go on".into(),
            mode: SendMode::Auto,
        }
    );
    // Ctrl+X twice stops the job.
    ui.key(key(KeyCode::Char('x'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err());
    ui.key(key(KeyCode::Char('x'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchCancel {
            session: "chat-session".into(),
            job: "job-3".into(),
        }
    );
    // The view's log is read on the next tick once the job has a run.
    ui.tick();
}

#[tokio::test]
async fn tabs_and_panes_move_with_the_bound_keys() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    ui.open_view(Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    });
    let agent = ui.active_tab().unwrap();
    let chat = ui.tabs[0].id;
    ui.key(key(KeyCode::Char('1'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.active_tab(), Some(chat));
    ui.key(key(KeyCode::Char('.'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.active_tab(), Some(agent));
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.panes.len(), 2);
    assert_eq!(ui.focus, Focus::Pane(1));
    assert_ne!(ui.panes[0], ui.panes[1]);
    let shown = screen(&ui, 160, 40);
    assert!(shown.contains("message"), "{shown}");
    ui.key(key(KeyCode::Char('o'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(0));
    // The last chat tab stays; an agent tab closes.
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.show(chat);
    ui.key(key(KeyCode::Char('w'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.tabs.len(), 2);
    ui.show(agent);
    ui.key(key(KeyCode::Char('w'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.tabs.len(), 1);
    // F1 opens the help, which lists the bindings; Esc closes it.
    ui.key(key(KeyCode::F(1), KeyModifiers::NONE))
        .await
        .unwrap();
    let shown = screen(&ui, 120, 40);
    assert!(shown.contains("Dashboard keys"), "{shown}");
    assert!(shown.contains("Ctrl+G"), "{shown}");
    ui.key(key(KeyCode::Esc, KeyModifiers::NONE)).await.unwrap();
    assert!(ui.overlay.is_none());
}

#[tokio::test]
async fn dashboard_commands_start_jobs_and_open_tabs_while_chat_text_still_goes_to_the_chat() {
    let (mut ui, mut requests, mut peer, _path) = dashboard().await;
    let type_line = |ui: &mut Ui, line: &str| {
        if let TabKind::Chat(chat) = &mut ui.tabs[0].kind {
            chat.app.input = line.into();
            chat.app.cursor = line.chars().count();
        }
    };
    type_line(&mut ui, "/job codex --worktree fix the race");
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchStart {
            workspace: "/tmp".into(),
            agent: "codex".into(),
            prompt: "fix the race".into(),
            cwd: None,
            model: None,
            effort: None,
            worktree: true,
        }
    );
    type_line(&mut ui, "/open scv");
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(ui.openings.len(), 1);
    assert_eq!(ui.openings[0].cwd, std::path::PathBuf::from("/tmp/scv"));
    // Anything else is the chat's.
    type_line(&mut ui, "hello there");
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut peer).await,
        ClientMessage::TurnStart { prompt, .. } if prompt == "hello there"
    ));
}

#[tokio::test]
async fn a_tab_in_the_background_that_asks_for_approval_raises_a_notice() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    ui.open_view(Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    });
    let chat = ui.tabs[0].id;
    ui.workbench.bell = false;
    ui.chat_event(
        chat,
        Ok(Some(ServerEvent::ApprovalRequested {
            request_id: "r".into(),
            session_id: "old-session".into(),
            turn_id: "t".into(),
            seq: 1,
            approval_id: "a".into(),
            call_id: "c".into(),
            name: "bash".into(),
            risk: "process".into(),
            cwd: "/tmp".into(),
            summary: "Run shell command: make".into(),
        })),
    )
    .unwrap();
    assert!(ui.workbench.bell);
    assert!(
        ui.workbench
            .notices
            .back()
            .unwrap()
            .text
            .starts_with("Tab 1 asks for approval")
    );
    // A small terminal shows the focused chat on its own.
    ui.show(chat);
    let shown = screen(&ui, 48, 10);
    assert!(shown.contains("approval required"), "{shown}");
}

fn running_job(session: &str, name: &str, created: u64) -> WorkbenchJob {
    let mut job = snapshot().jobs.remove(0);
    job.session = session.into();
    job.job = name.into();
    job.state = AgentState::Working;
    job.detail = None;
    job.created_unix_seconds = created;
    job
}

#[tokio::test]
async fn a_second_stop_press_counts_only_on_the_same_job_wherever_its_row_went() {
    let (mut ui, mut requests, _peer, _path) = dashboard().await;
    let mut two = snapshot();
    two.approvals.clear();
    two.jobs = vec![
        running_job("chat-session", "job-3", 3),
        running_job("chat-session", "job-4", 4),
    ];
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(two.clone())));
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    let select = |ui: &mut Ui, name: &str| {
        let items = ui.sidebar_items();
        let row = items
            .iter()
            .position(|item| matches!(item, Item::Job(job) if job.job == name))
            .unwrap();
        ui.workbench.select(&items, row);
    };
    select(&mut ui, "job-3");
    ui.key(key(KeyCode::Char('x'), KeyModifiers::NONE))
        .await
        .unwrap();
    // A press on another job starts its own confirmation.
    select(&mut ui, "job-4");
    ui.key(key(KeyCode::Char('x'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err(), "nothing stopped yet");
    // A job listed above it moves job-4's row; the selection follows it.
    two.jobs.insert(0, running_job("chat-session", "job-5", 5));
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(two)));
    ui.key(key(KeyCode::Char('x'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchCancel {
            session: "chat-session".into(),
            job: "job-4".into(),
        }
    );
}

#[tokio::test]
async fn a_tab_reconnects_once_however_many_keys_come_while_it_is_down() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    let chat = ui.tabs[0].id;
    if let TabKind::Chat(tab) = &mut ui.tabs[0].kind {
        tab.app.connected = false;
    }
    for character in "hello".chars() {
        ui.key(key(KeyCode::Char(character), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert_eq!(ui.openings.len(), 1);
    // Taken by the event loop, it is still under way: no second one.
    let opening = ui.openings.pop().unwrap();
    assert_eq!(opening.tab, Some(chat));
    assert_eq!(opening.restore.as_deref(), Some("old-session"));
    ui.chat_event(chat, Ok(None)).unwrap();
    ui.key(key(KeyCode::Char('x'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(ui.openings, []);
    // Once it failed, the next key tries again.
    ui.open_failed(&opening, &anyhow::anyhow!("refused"));
    ui.key(key(KeyCode::Char('x'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(ui.openings.len(), 1);
}

#[tokio::test]
async fn a_chats_approval_prompt_takes_its_answer_keys_wherever_focus_is() {
    let (mut ui, mut requests, mut peer, _path) = dashboard().await;
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    let items = ui.sidebar_items();
    let approval = items
        .iter()
        .position(|item| matches!(item, Item::Approval(_)))
        .unwrap();
    ui.workbench.select(&items, approval);
    if let TabKind::Chat(tab) = &mut ui.tabs[0].kind {
        tab.app.pending_approval = Some(crate::app::PendingApproval {
            id: "chat-approval".into(),
            name: "bash".into(),
            risk: "process".into(),
            cwd: "/tmp".into(),
            summary: "Run make".into(),
        });
    }
    // The prompt on screen is the chat's: y answers it, not the sidebar's
    // selected request.
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err());
    assert!(matches!(
        receive(&mut peer).await,
        ClientMessage::ApprovalResolve { approval_id, approved: true, .. } if approval_id == "chat-approval"
    ));
}

#[tokio::test]
async fn quitting_while_a_chat_has_jobs_running_asks_again() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    let mut busy = snapshot();
    busy.jobs = vec![running_job("old-session", "job-1", 1)];
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(busy)));
    ui.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(!ui.quit);
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("1 background job"), "{notice}");
    ui.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(ui.quit);
}

#[tokio::test]
async fn keys_never_go_to_a_sidebar_too_narrow_to_draw() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    ui.size = (60, 30);
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(0));
    assert!(
        ui.workbench
            .notices
            .back()
            .unwrap()
            .text
            .contains("70 columns")
    );
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    let TabKind::Chat(tab) = &ui.tabs[0].kind else {
        unreachable!()
    };
    assert_eq!(tab.app.input, "y", "typing went to the chat");
}

#[tokio::test]
async fn a_chat_drawn_alone_after_a_resize_is_the_one_its_keys_answer() {
    let (mut ui, _requests, mut first_peer, _path) = dashboard().await;
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let options = LaunchOptions::default();
    let (connection, mut second_peer) = tokio::join!(
        Client::connect(&path.0, Path::new("/tmp"), &options),
        accept_session(&listener, "second-session"),
    );
    let (client, session) = connection.unwrap();
    let opening = Opening {
        cwd: "/tmp".into(),
        restore: None,
        tab: None,
    };
    assert!(ui.opened(&opening, client, session).is_none());
    // The first chat in the first pane, the second in the focused second.
    ui.key(key(KeyCode::Char('1'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(1));
    for (index, tool) in ["first-tool", "second-tool"].into_iter().enumerate() {
        if let TabKind::Chat(tab) = &mut ui.tabs[index].kind {
            tab.app.pending_approval = Some(PendingApproval {
                id: format!("{tool}-approval"),
                name: tool.into(),
                risk: "process".into(),
                cwd: "/tmp".into(),
                summary: "Run make".into(),
            });
        }
    }
    // The terminal shrinks, as the event loop sees it before drawing: the
    // focused chat is drawn alone, and y answers its prompt.
    ui.size = (48, 10);
    ui.normalize_focus();
    let shown = screen(&ui, 48, 10);
    assert!(shown.contains("second-tool"), "{shown}");
    assert!(!shown.contains("first-tool"), "{shown}");
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut second_peer).await,
        ClientMessage::ApprovalResolve { approval_id, approved: true, .. } if approval_id == "second-tool-approval"
    ));
    let mut line = String::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), first_peer.read_line(&mut line))
            .await
            .is_err(),
        "the first chat was answered: {line}"
    );
}
