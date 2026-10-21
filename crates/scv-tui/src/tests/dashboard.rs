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
    ui::{ChatTab, Focus, TabKind, Ui},
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
    ui.workbench.selected = approval;
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
