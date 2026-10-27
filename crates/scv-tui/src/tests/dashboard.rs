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
        attach: None,
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
    // Plain y and n are text, even in an empty composer; Ctrl+N denies.
    for character in ['n', 'y'] {
        ui.key(key(KeyCode::Char(character), KeyModifiers::NONE))
            .await
            .unwrap();
    }
    assert!(requests.try_recv().is_err(), "typing answered the approval");
    ui.key(key(KeyCode::Esc, KeyModifiers::NONE)).await.unwrap();
    ui.key(key(KeyCode::Char('n'), KeyModifiers::CONTROL))
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
    );
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
    ui.chat_event(chat, Ok(None));
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
    // Keys the prompt does not answer with reach nothing behind it: the
    // sidebar neither opens, moves, nor lists anything.
    let tabs = ui.tabs.len();
    for code in [KeyCode::Enter, KeyCode::Char('w'), KeyCode::Down] {
        ui.key(key(code, KeyModifiers::NONE)).await.unwrap();
    }
    assert_eq!(ui.tabs.len(), tabs);
    assert_eq!(ui.overlay, None);
    assert_eq!(ui.workbench.selection(&ui.sidebar_items()), Some(approval));
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
        from: (ui.focus, ui.active_tab()),
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

#[tokio::test]
async fn a_key_meant_for_an_entry_that_went_away_only_moves_the_selection() {
    let (mut ui, mut requests, _peer, _path) = dashboard().await;
    let mut two = snapshot();
    let mut second = two.approvals[0].clone();
    second.id = "ap-2".into();
    second.job = "job-4".into();
    two.approvals.push(second);
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(two.clone())));
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    let items = ui.sidebar_items();
    let first = items
        .iter()
        .position(|item| matches!(item, Item::Approval(approval) if approval.id == "ap-1"))
        .unwrap();
    ui.workbench.select(&items, first);
    // ap-1 expires; ap-2 moves into its row.
    two.approvals.remove(0);
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(two)));
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        requests.try_recv().is_err(),
        "y meant for ap-1 approved ap-2"
    );
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("press again"), "{notice}");
    // Pressed again, it acts on what is selected now.
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchApprove {
            id: "ap-2".into(),
            approved: true
        }
    );
}

#[tokio::test]
async fn the_default_sidebar_highlight_does_not_act_after_its_entry_goes() {
    let (mut ui, mut requests, _peer, _path) = dashboard().await;
    let mut two = snapshot();
    let mut second = two.approvals[0].clone();
    second.id = "ap-2".into();
    second.summary = "rm -rf /".into();
    two.approvals.push(second);
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(two.clone())));
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    two.approvals.remove(0);
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(two)));
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(
        requests.try_recv().is_err(),
        "the default highlight approved the approval that moved into its row"
    );
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchApprove {
            id: "ap-2".into(),
            approved: true
        }
    );
}

#[tokio::test]
async fn a_new_tab_from_the_sidebar_takes_the_keys_that_type_its_path() {
    let (mut ui, mut requests, _peer, _path) = dashboard().await;
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(0));
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err(), "y answered the sidebar");
    let TabKind::Chat(tab) = &ui.tabs[0].kind else {
        unreachable!()
    };
    assert_eq!(tab.app.input, "/open y");
}

#[tokio::test]
async fn sidebar_focus_returns_to_the_pane_it_left_and_shows_pane_0s_approval() {
    let (mut ui, _requests, _first_peer, _path) = dashboard().await;
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let options = LaunchOptions::default();
    let (connection, _second_peer) = tokio::join!(
        Client::connect(&path.0, Path::new("/tmp"), &options),
        accept_session(&listener, "second-session"),
    );
    let (client, session) = connection.unwrap();
    let opening = Opening {
        cwd: "/tmp".into(),
        restore: None,
        tab: None,
        from: (ui.focus, ui.active_tab()),
    };
    assert!(ui.opened(&opening, client, session).is_none());
    ui.key(key(KeyCode::Char('1'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(1));
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    ui.key(key(KeyCode::Esc, KeyModifiers::NONE)).await.unwrap();
    assert_eq!(ui.focus, Focus::Pane(1));
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(1));
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    let first = ui.tabs[0].id;
    ui.workbench.bell = false;
    ui.chat_event(
        first,
        Ok(Some(ServerEvent::ApprovalRequested {
            request_id: "r".into(),
            session_id: "old-session".into(),
            turn_id: "t".into(),
            seq: 1,
            approval_id: "pane-0".into(),
            call_id: "c".into(),
            name: "bash".into(),
            risk: "process".into(),
            cwd: "/tmp".into(),
            summary: "Run make".into(),
        })),
    );
    assert!(!ui.workbench.bell, "the drawn prompt also rang");
    let shown = screen(&ui, 160, 40);
    assert!(shown.contains("approval required"), "{shown}");
    // The pane focus left is already asking, so pane 0's next request is
    // not drawn and is announced.
    if let TabKind::Chat(tab) = &mut ui.tabs[1].kind {
        tab.app.pending_approval = Some(PendingApproval {
            id: "pane-1".into(),
            name: "bash".into(),
            risk: "process".into(),
            cwd: "/tmp".into(),
            summary: "Run the other pane".into(),
        });
    }
    if let TabKind::Chat(tab) = &mut ui.tabs[0].kind {
        tab.app.pending_approval = None;
    }
    ui.workbench.bell = false;
    ui.chat_event(
        first,
        Ok(Some(ServerEvent::ApprovalRequested {
            request_id: "r2".into(),
            session_id: "old-session".into(),
            turn_id: "t".into(),
            seq: 2,
            approval_id: "pane-0-later".into(),
            call_id: "c2".into(),
            name: "bash".into(),
            risk: "process".into(),
            cwd: "/tmp".into(),
            summary: "Run make again".into(),
        })),
    );
    assert!(ui.workbench.bell);
    assert!(
        ui.workbench
            .notices
            .back()
            .unwrap()
            .text
            .contains("asks for approval")
    );
}

#[tokio::test]
async fn a_drawn_prompt_is_not_replaced_by_another_chats_request() {
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
        from: (ui.focus, ui.active_tab()),
    };
    assert!(ui.opened(&opening, client, session).is_none());
    ui.key(key(KeyCode::Char('1'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    let first = ui.tabs[0].id;
    let second = ui.tabs[1].id;
    let ask = |id: &str, session_id: &str, summary: &str| ServerEvent::ApprovalRequested {
        request_id: "r".into(),
        session_id: session_id.into(),
        turn_id: "t".into(),
        seq: 1,
        approval_id: id.into(),
        call_id: "c".into(),
        name: "bash".into(),
        risk: "process".into(),
        cwd: "/tmp".into(),
        summary: summary.into(),
    };
    ui.chat_event(first, Ok(Some(ask("a-1", "old-session", "Run make for A"))));
    ui.workbench.notices.clear();
    ui.workbench.bell = false;
    ui.chat_event(
        second,
        Ok(Some(ask("b-1", "second-session", "rm -rf ~ for B"))),
    );
    assert!(ui.workbench.bell);
    assert!(
        ui.workbench
            .notices
            .back()
            .unwrap()
            .text
            .contains("asks for approval")
    );
    let shown = screen(&ui, 160, 40);
    assert!(shown.contains("Run make for A"), "{shown}");
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut first_peer).await,
        ClientMessage::ApprovalResolve { approval_id, approved: true, .. } if approval_id == "a-1"
    ));
    let pending = tokio::time::timeout(Duration::from_millis(50), receive(&mut second_peer)).await;
    assert!(pending.is_err(), "the second chat was approved");
}

#[tokio::test]
async fn a_replaced_job_approval_is_not_answered_by_the_first_chord() {
    let (mut ui, mut requests, _peer, _path) = dashboard().await;
    let target = Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    };
    ui.open_view(target);
    // The open view notes ap-1. A later request on the same job does not
    // take the chord that was for ap-1.
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(snapshot())));
    let mut next = snapshot();
    next.approvals[0].id = "ap-2".into();
    next.approvals[0].summary = "rm -rf /".into();
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(next)));
    ui.key(key(KeyCode::Char('y'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(
        requests.try_recv().is_err(),
        "the first chord approved the new request"
    );
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(
        notice.contains("ap-2") && notice.contains("Ctrl+Y"),
        "{notice}"
    );
    ui.key(key(KeyCode::Char('y'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchApprove {
            id: "ap-2".into(),
            approved: true
        }
    );
}

#[tokio::test]
async fn an_agent_view_sends_one_prompt_at_a_time() {
    let (mut ui, mut requests, _peer, _path) = dashboard().await;
    let target = Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    };
    ui.open_view(target.clone());
    let type_and_send = |ui: &mut Ui, text: &str| {
        let view = ui.workbench.views.get_mut(&target).unwrap();
        view.input = text.into();
        view.cursor = text.chars().count();
    };
    type_and_send(&mut ui, "first");
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchSend { prompt, .. } if prompt == "first"
    ));
    // While it is unanswered, a second prompt stays in the composer.
    type_and_send(&mut ui, "second");
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err());
    assert_eq!(ui.workbench.views[&target].input, "second");
    assert_eq!(ui.workbench.views[&target].sent.as_deref(), Some("first"));
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("Still sending"), "{notice}");
    // Once the first is answered, it goes.
    ui.workbench.answer(Answer::Status(
        Tag::Send(target.clone()),
        status(snapshot()),
    ));
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        requests.try_recv().unwrap().command,
        DaemonCommand::WorkbenchSend { prompt, .. } if prompt == "second"
    ));
}

#[tokio::test]
async fn a_tabs_unreadable_frame_disconnects_only_that_tab() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    let chat = ui.tabs[0].id;
    ui.chat_event(chat, Err(anyhow!("server frame exceeds 65536 bytes")));
    let TabKind::Chat(tab) = &ui.tabs[0].kind else {
        unreachable!()
    };
    assert!(!tab.app.connected);
    assert!(tab.app.items.iter().any(|item| matches!(
        item,
        TranscriptItem::Error(text) if text.contains("exceeds 65536 bytes")
    )));
    assert!(!ui.quit);
    // It reconnects as a lost connection does.
    assert_eq!(ui.openings.len(), 1);
    assert_eq!(ui.openings[0].tab, Some(chat));
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("connection failed"), "{notice}");
}

#[tokio::test]
async fn quitting_with_prompts_queued_asks_again() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    if let TabKind::Chat(tab) = &mut ui.tabs[0].kind {
        tab.app.queue.push_back(scv_protocol::QueueEntry {
            queue_id: "q-1".into(),
            revision: 1,
            prompt: "Run the release checklist.".into(),
            submitter: "tui".into(),
            attachments: Vec::new(),
        });
        tab.app.queue_paused = true;
    }
    ui.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(!ui.quit);
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("tab 1 has 1 queued prompt(s)"), "{notice}");
    ui.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(ui.quit);
}

#[tokio::test]
async fn a_slow_new_tab_does_not_take_a_pane_the_owner_moved_away_from() {
    let (mut ui, _requests, _first_peer, _path) = dashboard().await;
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let options = LaunchOptions::default();
    let (connection, _second_peer) = tokio::join!(
        Client::connect(&path.0, Path::new("/tmp"), &options),
        accept_session(&listener, "second-session"),
    );
    let (client, session) = connection.unwrap();
    // Asked for from the chat, which the owner then left for an agent view.
    let opening = Opening {
        cwd: "/w/shop".into(),
        restore: None,
        tab: None,
        from: (ui.focus, ui.active_tab()),
    };
    ui.open_view(Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    });
    let shown = ui.active_tab();
    assert!(ui.opened(&opening, client, session).is_none());
    assert_eq!(ui.tabs.len(), 3);
    assert_eq!(ui.active_tab(), shown, "the new tab took the focused pane");
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("Tab 3 is open"), "{notice}");
}

#[tokio::test]
async fn enter_on_a_tab_the_second_pane_shows_focuses_that_pane() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    ui.open_view(Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    });
    let chat = ui.tabs[0].id;
    // The agent view in the first pane, the chat in the second.
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.panes[1], chat);
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    let items = ui.sidebar_items();
    let row = items
        .iter()
        .position(|item| matches!(item, Item::Tab(id, _) if *id == chat))
        .unwrap();
    ui.workbench.select(&items, row);
    let panes = ui.panes.clone();
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(ui.panes, panes);
    assert_eq!(ui.focus, Focus::Pane(1));
}

#[tokio::test]
async fn a_sidebar_key_leaves_the_approval_prompt_on_its_chat() {
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
        from: (ui.focus, ui.active_tab()),
    };
    assert!(ui.opened(&opening, client, session).is_none());
    ui.key(key(KeyCode::Char('1'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(1));
    if let TabKind::Chat(tab) = &mut ui.tabs[1].kind {
        tab.app.pending_approval = Some(PendingApproval {
            id: "pane-1-approval".into(),
            name: "bash".into(),
            risk: "process".into(),
            cwd: "/tmp".into(),
            summary: "Run make".into(),
        });
    }
    // Ctrl+G is a dashboard key, so it is allowed while the prompt is up.
    // The prompt stays the second chat's.
    ui.key(key(KeyCode::Char('g'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Sidebar);
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut second_peer).await,
        ClientMessage::ApprovalResolve { approval_id, approved: true, .. }
            if approval_id == "pane-1-approval"
    ));
    if let TabKind::Chat(tab) = &ui.tabs[0].kind {
        assert!(
            tab.app.input.is_empty(),
            "pane 0 took the answer: {}",
            tab.app.input
        );
        assert!(tab.app.pending_approval.is_none());
    }
    let pending = tokio::time::timeout(Duration::from_millis(50), receive(&mut first_peer)).await;
    assert!(pending.is_err(), "the first chat was answered");
}

#[tokio::test]
async fn ctrl_c_after_quit_confirms_instead_of_cancelling_the_turn() {
    let (mut ui, _requests, mut peer, _path) = dashboard().await;
    if let TabKind::Chat(tab) = &mut ui.tabs[0].kind {
        tab.app.turn = Some(ActiveTurn {
            id: Some("turn-1".into()),
            started_at: Instant::now(),
        });
        tab.app.input = "/quit".into();
        tab.app.cursor = tab.app.input.chars().count();
    }
    ui.key(key(KeyCode::Enter, KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(!ui.quit);
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("Ctrl+C"), "{notice}");
    if let TabKind::Chat(tab) = &ui.tabs[0].kind {
        assert!(tab.app.input.is_empty(), "{}", tab.app.input);
        assert!(tab.app.turn.is_some());
    }
    ui.key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(ui.quit);
    let pending = tokio::time::timeout(Duration::from_millis(50), receive(&mut peer)).await;
    assert!(pending.is_err(), "Ctrl+C cancelled the turn");
}

#[tokio::test]
async fn sidebar_text_from_the_daemon_is_drawn_without_terminal_controls() {
    let (mut ui, _requests, _peer, _path) = dashboard().await;
    if let TabKind::Chat(tab) = &mut ui.tabs[0].kind {
        tab.app.model = "gpt\u{1b}[2J".into();
    }
    let mut hostile = snapshot();
    hostile.jobs[0].workspace = "/w/\u{1b}]0;owned\u{7}repo".into();
    hostile.jobs[0].worktree = Some(scv_protocol::WorktreeRef {
        path: "/w/wt".into(),
        branch: "scv/\u{9b}31mred".into(),
        repo: "/w/repo".into(),
    });
    hostile.approvals[0].job = "job-\u{1b}[2J".into();
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(hostile)));
    let controls = |shown: &str| shown.chars().any(|c| c != '\n' && c.is_control());
    let shown = screen(&ui, 160, 36);
    assert!(!controls(&shown), "{shown:?}");
    assert!(shown.contains("\u{fffd}]0;owned"), "{shown}");
    assert!(shown.contains("gpt\u{fffd}[2J"), "{shown}");
    ui.open_view(Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    });
    let shown = screen(&ui, 160, 36);
    assert!(!controls(&shown), "{shown:?}");
    assert!(shown.contains("scv/\u{fffd}31mred"), "{shown}");
    assert_eq!(
        crate::render::clean("a\u{1b}b\u{9b}c\u{7f}d"),
        "a\u{fffd}b\u{fffd}c\u{fffd}d"
    );
}

async fn second_chat(ui: &mut Ui) -> BufReader<UnixStream> {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let options = LaunchOptions::default();
    let (connection, peer) = tokio::join!(
        Client::connect(&path.0, Path::new("/tmp"), &options),
        accept_session(&listener, "second-session"),
    );
    let (client, session) = connection.unwrap();
    let opening = Opening {
        cwd: "/tmp".into(),
        restore: None,
        tab: None,
        from: (ui.focus, ui.active_tab()),
    };
    assert!(ui.opened(&opening, client, session).is_none());
    peer
}

fn ask(id: &str, session_id: &str, summary: &str) -> ServerEvent {
    ServerEvent::ApprovalRequested {
        request_id: "r".into(),
        session_id: session_id.into(),
        turn_id: "t".into(),
        seq: 1,
        approval_id: id.into(),
        call_id: "c".into(),
        name: "bash".into(),
        risk: "process".into(),
        cwd: "/tmp".into(),
        summary: summary.into(),
    }
}

#[tokio::test]
async fn a_pinned_prompt_is_drawn_and_answered_in_a_small_terminal() {
    let (mut ui, _requests, mut first_peer, _path) = dashboard().await;
    let mut second_peer = second_chat(&mut ui).await;
    let first = ui.tabs[0].id;
    ui.size = (48, 10);
    ui.key(key(KeyCode::Char('1'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.chat_event(first, Ok(Some(ask("a-1", "old-session", "Run make for A"))));
    // Alt+2 shows the other chat. The prompt stays A's, and the small
    // terminal still draws it.
    ui.key(key(KeyCode::Char('2'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.active_tab(), Some(ui.tabs[1].id));
    let shown = screen(&ui, 48, 10);
    // Ten rows clip the summary; the prompt itself is what must be on screen.
    assert!(shown.contains("approval required"), "{shown}");
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut first_peer).await,
        ClientMessage::ApprovalResolve { approval_id, approved: true, .. } if approval_id == "a-1"
    ));
    if let TabKind::Chat(tab) = &ui.tabs[1].kind {
        assert!(
            tab.app.input.is_empty(),
            "the drawn chat took y: {}",
            tab.app.input
        );
    }
    let pending = tokio::time::timeout(Duration::from_millis(50), receive(&mut second_peer)).await;
    assert!(pending.is_err(), "the second chat was approved");
}

#[tokio::test]
async fn a_pinned_prompt_stays_drawn_after_focus_moves_and_the_terminal_shrinks() {
    let (mut ui, _requests, mut first_peer, _path) = dashboard().await;
    let mut second_peer = second_chat(&mut ui).await;
    let first = ui.tabs[0].id;
    ui.size = (160, 40);
    ui.key(key(KeyCode::Char('1'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('o'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(0));
    ui.chat_event(first, Ok(Some(ask("a-1", "old-session", "Run make for A"))));
    // Wide: focus moves to the other pane while the prompt stays pinned.
    ui.key(key(KeyCode::Char('o'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.focus, Focus::Pane(1));
    ui.size = (48, 10);
    let shown = screen(&ui, 48, 10);
    assert!(shown.contains("approval required"), "{shown}");
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut first_peer).await,
        ClientMessage::ApprovalResolve { approval_id, approved: true, .. } if approval_id == "a-1"
    ));
    let pending = tokio::time::timeout(Duration::from_millis(50), receive(&mut second_peer)).await;
    assert!(pending.is_err(), "the second chat was approved");
}

#[tokio::test]
async fn a_request_from_the_focused_chat_under_a_pinned_prompt_is_announced() {
    let (mut ui, _requests, mut first_peer, _path) = dashboard().await;
    let mut second_peer = second_chat(&mut ui).await;
    let first = ui.tabs[0].id;
    let second = ui.tabs[1].id;
    ui.size = (160, 40);
    ui.key(key(KeyCode::Char('1'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('v'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('o'), KeyModifiers::ALT))
        .await
        .unwrap();
    ui.chat_event(first, Ok(Some(ask("a-1", "old-session", "Run make for A"))));
    ui.key(key(KeyCode::Char('o'), KeyModifiers::ALT))
        .await
        .unwrap();
    assert_eq!(ui.active_tab(), Some(second));
    ui.workbench.notices.clear();
    ui.workbench.bell = false;
    ui.chat_event(
        second,
        Ok(Some(ask("b-1", "second-session", "rm -rf ~ for B"))),
    );
    assert!(ui.workbench.bell);
    assert!(
        ui.workbench
            .notices
            .back()
            .unwrap()
            .text
            .contains("asks for approval")
    );
    let shown = screen(&ui, 160, 40);
    assert!(shown.contains("Run make for A"), "{shown}");
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut first_peer).await,
        ClientMessage::ApprovalResolve { approval_id, approved: true, .. } if approval_id == "a-1"
    ));
    let pending = tokio::time::timeout(Duration::from_millis(50), receive(&mut second_peer)).await;
    assert!(pending.is_err(), "the second chat was approved");
}

/// The dashboard with job-3's view open and drawn: its dashboard
/// requests, its attached terminals' reads, keys, and detaches, and its
/// takeovers' starts and ends come out of the three receivers.
struct Attachable {
    ui: Ui,
    requests: mpsc::UnboundedReceiver<Request>,
    terminal: mpsc::UnboundedReceiver<Request>,
    takeover: mpsc::UnboundedReceiver<Request>,
    target: Target,
    _peer: BufReader<UnixStream>,
    _path: SocketPath,
}

async fn attachable() -> Attachable {
    let (mut ui, requests, peer, path) = dashboard().await;
    let (terminal, terminal_requests) = mpsc::unbounded_channel();
    ui.terminal_requests = Some(terminal);
    let (takeover, takeover_requests) = mpsc::unbounded_channel();
    ui.takeover_requests = Some(takeover);
    let target = Target::Job {
        session: "chat-session".into(),
        job: "job-3".into(),
    };
    ui.open_view(target.clone());
    ui.size = (120, 36);
    screen(&ui, 120, 36);
    Attachable {
        ui,
        requests,
        terminal: terminal_requests,
        takeover: takeover_requests,
        target,
        _peer: peer,
        _path: path,
    }
}

fn attach_page(id: &str, generation: u64, text: &str) -> scv_protocol::AttachPage {
    scv_protocol::AttachPage {
        id: id.into(),
        session: "chat-session".into(),
        conversation: "codex-2".into(),
        agent: "codex".into(),
        run: Some("codex-9f3e21".into()),
        generation,
        cols: 80,
        rows: 10,
        lines: vec![scv_protocol::AttachRow {
            row: 0,
            spans: vec![scv_protocol::AttachSpan {
                text: text.into(),
                ..scv_protocol::AttachSpan::default()
            }],
        }],
        full: true,
        cursor: Some([0, 5]),
        application_cursor: false,
        bracketed_paste: false,
        ended: false,
        exit: None,
        note: None,
    }
}

fn attach_status(page: scv_protocol::AttachPage) -> Box<DaemonStatus> {
    let mut status = status(snapshot());
    status.workbench = None;
    status.attach = Some(page);
    status
}

#[tokio::test]
async fn ctrl_o_attaches_and_every_key_but_detach_goes_to_the_agent() {
    let Attachable {
        mut ui,
        mut requests,
        mut terminal,
        mut takeover,
        target,
        ..
    } = attachable().await;
    ui.key(key(KeyCode::Char('o'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    // A takeover starts on a connection of its own.
    assert!(terminal.try_recv().is_err());
    let Request { tag, command } = takeover.try_recv().unwrap();
    assert_eq!(tag, Tag::Attach(target.clone()));
    let DaemonCommand::Attach {
        session,
        conversation,
        cols,
        rows,
    } = command
    else {
        panic!("{command:?}");
    };
    assert_eq!(
        (session.as_str(), conversation.as_str()),
        ("chat-session", "codex-2")
    );
    // The terminal takes the view's size.
    assert!(cols.is_some_and(|cols| cols > 40) && rows.is_some_and(|rows| rows > 10));
    let note = ui
        .workbench
        .terminal_answer(Answer::Status(
            Tag::Attach(target.clone()),
            attach_status(attach_page("at-1", 1, "codex> ready")),
        ))
        .unwrap();
    assert!(note.contains("Ctrl+]"), "{note}");
    let shown = screen(&ui, 120, 36);
    assert!(shown.contains("codex> ready"), "{shown}");
    assert!(shown.contains("own terminal, taken over"), "{shown}");

    // A dashboard key (new tab) is the agent's while attached.
    let tabs = ui.tabs.len();
    ui.key(key(KeyCode::Char('t'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert_eq!(ui.tabs.len(), tabs);
    let Request { command, .. } = terminal.try_recv().unwrap();
    assert!(
        matches!(&command, DaemonCommand::AttachIo { id, input, .. } if id == "at-1" && input == "\x14"),
        "{command:?}"
    );
    // So are the view's own approval chords: an approval waits on job-3,
    // and Ctrl+Y typed into the terminal answers nothing.
    ui.key(key(KeyCode::Char('y'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    ui.key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();
    assert!(requests.try_recv().is_err());
    ui.workbench.terminal_answer(Answer::Status(
        Tag::AttachIo(target.clone()),
        attach_status(attach_page("at-1", 2, "codex> y")),
    ));
    ui.terminal_tick();
    let Request { command, .. } = terminal.try_recv().unwrap();
    assert!(
        matches!(&command, DaemonCommand::AttachIo { input, after: 2, .. } if input == "\x19y"),
        "{command:?}"
    );
    ui.workbench.terminal_answer(Answer::Status(
        Tag::AttachIo(target.clone()),
        attach_status(attach_page("at-1", 3, "codex> y")),
    ));

    ui.key(key(KeyCode::Char(']'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(ui.workbench.views[&target].terminal.is_none());
    assert!(matches!(
        terminal.try_recv().unwrap().command,
        DaemonCommand::AttachDetach { id } if id == "at-1"
    ));
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("keeps running"), "{notice}");
    // Detached, the keys are the dashboard's and the view's again.
    let shown = screen(&ui, 120, 36);
    assert!(shown.contains("send to codex-2"), "{shown}");
}

#[tokio::test]
async fn a_refused_attach_keeps_the_activity_view_and_says_why() {
    let Attachable {
        mut ui,
        mut takeover,
        target,
        ..
    } = attachable().await;
    ui.key(key(KeyCode::Char('o'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(takeover.try_recv().is_ok());
    let note = ui
        .workbench
        .terminal_answer(Answer::Refused(
            Tag::Attach(target.clone()),
            "claude cannot be taken over in its terminal: …".into(),
        ))
        .unwrap();
    assert!(note.contains("cannot be taken over"), "{note}");
    let view = &ui.workbench.views[&target];
    assert!(view.terminal.is_none() && !view.attaching);
    // An older daemon that does not know the action keeps today's view.
    ui.key(key(KeyCode::Char('o'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(takeover.try_recv().is_ok());
    let note = ui.workbench.terminal_answer(Answer::Unsupported).unwrap();
    assert!(note.contains("predates attach"), "{note}");
    assert!(ui.workbench.views[&target].terminal.is_none());
    assert_eq!(
        ui.workbench.availability,
        crate::workbench::Availability::Ready
    );
    let shown = screen(&ui, 120, 36);
    assert!(shown.contains("send to codex-2"), "{shown}");
}

#[tokio::test]
async fn a_terminal_that_closes_or_loses_the_daemon_falls_back_to_the_activity_view() {
    let Attachable {
        mut ui,
        mut takeover,
        target,
        ..
    } = attachable().await;
    ui.key(key(KeyCode::Char('o'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    takeover.try_recv().unwrap();
    ui.workbench.terminal_answer(Answer::Status(
        Tag::Attach(target.clone()),
        attach_status(attach_page("at-1", 1, "bye")),
    ));
    let mut ended = attach_page("at-1", 2, "bye");
    ended.ended = true;
    ended.exit = Some("exit 0".into());
    let note = ui
        .workbench
        .terminal_answer(Answer::Status(
            Tag::AttachIo(target.clone()),
            attach_status(ended),
        ))
        .unwrap();
    assert!(note.contains("closed (exit 0)"), "{note}");
    assert!(ui.workbench.views[&target].terminal.is_none());
    // Attached again, the daemon goes away.
    ui.key(key(KeyCode::Char('o'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    takeover.try_recv().unwrap();
    ui.workbench.terminal_answer(Answer::Status(
        Tag::Attach(target.clone()),
        attach_status(attach_page("at-2", 1, "again")),
    ));
    let note = ui
        .workbench
        .terminal_answer(Answer::Disconnected(Tag::AttachIo(target.clone())))
        .unwrap();
    assert!(note.contains("detached"), "{note}");
    assert!(ui.workbench.views[&target].terminal.is_none());
}

#[tokio::test]
async fn a_view_closed_while_attached_detaches_from_its_terminal() {
    let Attachable {
        mut ui,
        mut terminal,
        mut takeover,
        target,
        ..
    } = attachable().await;
    ui.key(key(KeyCode::Char('o'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    takeover.try_recv().unwrap();
    ui.workbench.terminal_answer(Answer::Status(
        Tag::Attach(target.clone()),
        attach_status(attach_page("at-1", 1, "x")),
    ));
    ui.tabs
        .retain(|tab| !matches!(&tab.kind, TabKind::Agent(open) if *open == target));
    ui.tick();
    let mut detached = false;
    while let Ok(Request { command, .. }) = terminal.try_recv() {
        detached |= matches!(command, DaemonCommand::AttachDetach { id } if id == "at-1");
    }
    assert!(detached);
}

#[tokio::test]
async fn ctrl_x_twice_ends_a_takeover_the_view_follows() {
    let Attachable {
        mut ui,
        mut terminal,
        mut takeover,
        ..
    } = attachable().await;
    let mut taken = snapshot();
    taken.runs.push(scv_protocol::WorkbenchRun {
        handle: "codex-9f3e21".into(),
        agent: "codex".into(),
        session: "chat-session".into(),
        transport: "terminal".into(),
        conversation: Some("codex-2".into()),
        turn: Some(1),
        job: None,
        jobs: Vec::new(),
        cwd: "/w/projects/shop".into(),
        state: AgentState::Working,
        detail: Some("taken over in its terminal; nobody is attached".into()),
        started_unix_seconds: 5,
        last_activity_unix_seconds: 5,
        live: true,
        steerable: false,
    });
    ui.workbench
        .answer(Answer::Status(Tag::Snapshot, status(taken)));
    let view = Target::Conversation {
        session: "chat-session".into(),
        conversation: "codex-2".into(),
    };
    ui.open_view(view.clone());
    ui.key(key(KeyCode::Char('x'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(takeover.try_recv().is_err());
    let notice = &ui.workbench.notices.back().unwrap().text;
    assert!(notice.contains("end the takeover of codex-2"), "{notice}");
    ui.key(key(KeyCode::Char('x'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    assert!(matches!(
        takeover.try_recv().unwrap().command,
        DaemonCommand::AttachEnd { session, conversation }
            if session == "chat-session" && conversation == "codex-2"
    ));
    // Ending it never holds up an attached terminal's reads and keys.
    assert!(terminal.try_recv().is_err());
}
