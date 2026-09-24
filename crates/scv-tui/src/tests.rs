//! Tests of the terminal UI and its server connection, driven through a
//! local socket and a test terminal.

use std::io::Cursor;
use std::path::PathBuf;

use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use scv_protocol::{ClientMessage, PROTOCOL_VERSION, PeerInfo, QueueEntry, ServerEvent};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

use super::*;
use crate::{
    app::{ActiveTurn, App, PendingApproval},
    client::*,
    input::*,
    render::render,
    transcript::{ToolStatus, TranscriptItem},
};
use ratatui::backend::TestBackend;
use tokio::net::UnixListener;

struct SocketPath(PathBuf);

impl SocketPath {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("scv-tui-{}.sock", new_id())))
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn receive(peer: &mut BufReader<UnixStream>) -> ClientMessage {
    let mut line = String::new();
    let count = tokio::time::timeout(Duration::from_secs(2), peer.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(count, 0, "client closed before sending a message");
    serde_json::from_str(&line).unwrap()
}

async fn emit(peer: &mut BufReader<UnixStream>, event: ServerEvent) {
    let mut bytes = serde_json::to_vec(&event).unwrap();
    bytes.push(b'\n');
    peer.get_mut().write_all(&bytes).await.unwrap();
}

async fn accept_session(listener: &UnixListener, id: &str) -> BufReader<UnixStream> {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(6), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut peer = BufReader::new(stream);
    assert!(matches!(
        receive(&mut peer).await,
        ClientMessage::Initialize { .. }
    ));
    emit(
        &mut peer,
        ServerEvent::Initialized {
            request_id: "initialize".into(),
            protocol_version: PROTOCOL_VERSION,
            server: PeerInfo {
                name: "test".into(),
                version: "0".into(),
            },
        },
    )
    .await;
    assert!(matches!(
        receive(&mut peer).await,
        ClientMessage::SessionStart { .. }
    ));
    emit(
        &mut peer,
        ServerEvent::SessionStarted {
            request_id: "session-start".into(),
            session_id: id.into(),
            cwd: "/tmp".into(),
            model: "test".into(),
            context_max_tokens: 100,
            max_server_frame_bytes: 65536,
            max_transcript_bytes: 16384,
            max_transcript_items: 100,
            max_prompt_history_bytes: 1024,
            max_prompt_history_items: 10,
        },
    )
    .await;
    peer
}

async fn connected(
    path: &SocketPath,
    listener: &UnixListener,
) -> (Client, App, BufReader<UnixStream>) {
    let options = LaunchOptions::default();
    let (connection, peer) = tokio::join!(
        Client::connect_at(&path.0, Path::new("/tmp"), &options),
        accept_session(listener, "old-session"),
    );
    let (client, session) = connection.unwrap();
    (client, App::new(session), peer)
}

fn pending_work(app: &mut App) {
    app.turn = Some(ActiveTurn {
        id: Some("old-turn".into()),
        started_at: Instant::now(),
    });
    app.pending_approval = Some(PendingApproval {
        id: "old-approval".into(),
        name: "test".into(),
        risk: "high".into(),
        cwd: "/tmp".into(),
        summary: "pending".into(),
    });
    app.queue.push_back(QueueEntry {
        queue_id: "old-queue".into(),
        revision: 1,
        prompt: "queued work".into(),
        submitter: "test".into(),
        attachments: Vec::new(),
    });
    app.queue_paused = true;
    app.queue_selected = Some(0);
    app.queue_editing = app.queue.front().cloned();
    app.input = "edited queue prompt".into();
    app.cursor = app.input.len();
    app.context_after_tokens = Some(42);
    app.history_bytes = Some(100);
    app.last_seq = 9;
    app.push_item(TranscriptItem::Tool {
        call_id: "old-tool".into(),
        name: "test".into(),
        status: ToolStatus::Running,
        arguments: String::new(),
        output: String::new(),
        progress: String::new(),
        expanded: false,
    });
    app.push_item(TranscriptItem::Assistant {
        content: "partial answer".into(),
        streaming: true,
    });
}

fn assert_disconnected(app: &App) {
    assert!(!app.connected);
    assert!(app.turn.is_none());
    assert!(app.pending_approval.is_none());
    assert!(app.queue.is_empty());
    assert!(!app.queue_paused);
    assert!(app.queue_editing.is_none());
    assert!(app.queue_selected.is_none());
    assert!(app.context_after_tokens.is_none());
    assert!(app.history_bytes.is_none());
    assert_eq!(app.last_seq, 0);
    assert!(!app.items.iter().any(|item| matches!(
        item,
        TranscriptItem::Assistant {
            streaming: true,
            ..
        } | TranscriptItem::Tool {
            status: ToolStatus::Running | ToolStatus::Approval | ToolStatus::Proposed,
            ..
        }
    )));
}

async fn assert_fresh_session_no_replay(
    client: &mut Client,
    app: &mut App,
    peer: &mut BufReader<UnixStream>,
) {
    assert!(app.connected);
    assert_eq!(app.session_id, "new-session");
    assert!(app.items.iter().any(|item| matches!(item,
        TranscriptItem::System(text) if text.contains("fresh session") && text.contains("history was not restored")
    )));
    handle_key(
        client,
        app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .unwrap();
    app.input = "fresh prompt".into();
    handle_key(
        client,
        app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .unwrap();
    assert!(
        matches!(receive(peer).await, ClientMessage::TurnStart { session_id, prompt, .. }
        if session_id == "new-session" && prompt == "fresh prompt")
    );
    let mut line = String::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), peer.read_line(&mut line))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn connected_client_reconnects_after_daemon_restart_without_replaying_work() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let (mut client, mut app, mut peer) = connected(&path, &listener).await;
    app.input = "interrupted prompt".into();
    handle_key(
        &mut client,
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .unwrap();
    assert!(matches!(
        receive(&mut peer).await,
        ClientMessage::TurnStart { .. }
    ));
    pending_work(&mut app);
    drop(peer);
    drop(listener);
    std::fs::remove_file(&path.0).unwrap();
    app.handle_server_result(client.read_event().await).unwrap();
    assert_disconnected(&app);
    assert!(app.input.is_empty());
    assert_eq!(app.prompt_history.front().unwrap(), "interrupted prompt");
    handle_key(
        &mut client,
        &mut app,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    )
    .await
    .unwrap();

    let options = LaunchOptions::default();
    let (connection, mut peer) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            reconnect_client(&path.0, Path::new("/tmp"), &options),
            async {
                // The first retry sees a missing socket, as during a restart.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let listener = UnixListener::bind(&path.0).unwrap();
                accept_session(&listener, "new-session").await
            }
        )
    })
    .await
    .unwrap();
    let (new_client, session) = connection;
    client = new_client;
    app.reconnect(session);
    assert_fresh_session_no_replay(&mut client, &mut app, &mut peer).await;
}

#[tokio::test]
async fn socket_write_failures_clear_pending_commands_and_never_replay_them() {
    for action in ["prompt", "queue", "approval", "cancel", "clear"] {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.0).unwrap();
        let (mut client, mut app, peer) = connected(&path, &listener).await;
        pending_work(&mut app);
        let key = match action {
            "approval" => KeyCode::Char('y'),
            "cancel" => {
                app.pending_approval = None;
                KeyCode::Esc
            }
            _ => {
                app.pending_approval = None;
                if action != "queue" {
                    app.queue_editing = None;
                }
                app.input = if action == "clear" {
                    "/clear"
                } else {
                    "interrupted prompt"
                }
                .into();
                KeyCode::Enter
            }
        };
        drop(peer);
        handle_key(
            &mut client,
            &mut app,
            KeyEvent::new(key, KeyModifiers::NONE),
        )
        .await
        .unwrap();
        assert_disconnected(&app);
        assert!(app.input.is_empty(), "{action}");
        let options = LaunchOptions::default();
        let ((new_client, session), mut peer) = tokio::join!(
            reconnect_client(&path.0, Path::new("/tmp"), &options),
            accept_session(&listener, "new-session"),
        );
        client = new_client;
        app.reconnect(session);
        assert_fresh_session_no_replay(&mut client, &mut app, &mut peer).await;
    }
}

#[tokio::test]
async fn stalled_socket_write_times_out_without_replaying_a_partial_prompt() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let (mut client, mut app, peer) = connected(&path, &listener).await;
    // The peer stays alive without draining its socket, forcing a partial write.
    app.input = "x".repeat(8 * 1024 * 1024);
    tokio::time::timeout(
        SOCKET_TIMEOUT + Duration::from_secs(2),
        handle_key(
            &mut client,
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_disconnected(&app);
    assert!(app.input.is_empty());
    client.shutdown().await;
    drop(peer);
    let options = LaunchOptions::default();
    let ((new_client, session), mut peer) = tokio::join!(
        reconnect_client(&path.0, Path::new("/tmp"), &options),
        accept_session(&listener, "new-session"),
    );
    client = new_client;
    app.reconnect(session);
    assert_fresh_session_no_replay(&mut client, &mut app, &mut peer).await;
}

#[tokio::test]
async fn partial_socket_eof_is_recoverable_and_preserves_unsent_draft() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let (mut client, mut app, mut peer) = connected(&path, &listener).await;
    app.input = "unsent draft".into();
    peer.get_mut().write_all(b"{\"type\":").await.unwrap();
    drop(peer);
    let error = client.read_event().await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::UnexpectedEof
    );
    app.handle_server_result(Err(error)).unwrap();
    assert_disconnected(&app);
    assert_eq!(app.input, "unsent draft");
    handle_key(
        &mut client,
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await
    .unwrap();
    assert!(app.quit);
}

#[tokio::test]
async fn socket_frames_survive_cancelled_reads_and_protocol_errors_are_not_retried() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let (mut client, mut app, mut peer) = connected(&path, &listener).await;
    let event = ServerEvent::QueueSnapshot {
        request_id: None,
        session_id: app.session_id.clone(),
        seq: 1,
        entries: vec![],
        paused: false,
    };
    let bytes = serde_json::to_vec(&event).unwrap();
    let middle = bytes.len() / 2;
    peer.get_mut().write_all(&bytes[..middle]).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), client.read_event())
            .await
            .is_err()
    );
    peer.get_mut().write_all(&bytes[middle..]).await.unwrap();
    peer.get_mut().write_all(b"\n").await.unwrap();
    assert_eq!(client.read_event().await.unwrap(), Some(event));
    peer.get_mut().write_all(b"not json\n").await.unwrap();
    assert!(app.handle_server_result(client.read_event().await).is_err());
    assert!(app.connected);
}

#[tokio::test]
async fn reconnect_retries_a_stalled_handshake_with_a_bounded_wait() {
    let path = SocketPath::new();
    let listener = UnixListener::bind(&path.0).unwrap();
    let options = LaunchOptions::default();
    let ((_, session), _peer) = tokio::time::timeout(
        SOCKET_TIMEOUT + RECONNECT_DELAY + Duration::from_secs(2),
        async {
            tokio::join!(
                reconnect_client(&path.0, Path::new("/tmp"), &options),
                async {
                    let (stalled, _) = listener.accept().await.unwrap();
                    let mut stalled = BufReader::new(stalled);
                    assert!(matches!(
                        receive(&mut stalled).await,
                        ClientMessage::Initialize { .. }
                    ));
                    let peer = accept_session(&listener, "new-session").await;
                    let mut line = String::new();
                    assert_eq!(stalled.read_line(&mut line).await.unwrap(), 0);
                    peer
                }
            )
        },
    )
    .await
    .unwrap();
    assert_eq!(session.id, "new-session");
}

#[test]
fn transient_io_errors_are_distinct_from_protocol_errors() {
    for kind in [
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::NotConnected,
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::TimedOut,
    ] {
        assert!(is_transport_error(
            &anyhow::Error::new(io::Error::from(kind)).context("socket")
        ));
    }
    assert!(!is_transport_error(&anyhow!("invalid protocol event")));
    assert!(!is_transport_error(
        &io::Error::from(io::ErrorKind::PermissionDenied).into()
    ));
}

fn app() -> App {
    App::new(SessionInfo {
        id: "s".into(),
        cwd: "/tmp".into(),
        model: "test".into(),
        context_max_tokens: 100,
        max_server_frame_bytes: DEFAULT_SERVER_FRAME_LIMIT,
        max_transcript_bytes: 20,
        max_transcript_items: 3,
        max_prompt_history_bytes: 10,
        max_prompt_history_items: 2,
    })
}

#[test]
fn prompt_history_is_bounded() {
    let mut app = app();
    app.add_prompt_history("one".into());
    app.add_prompt_history("two".into());
    app.add_prompt_history("three".into());
    assert_eq!(app.prompt_history.len(), 2);
    assert_eq!(app.prompt_history.front().unwrap(), "two");
}

#[test]
fn editor_handles_unicode_boundaries() {
    let mut app = app();
    insert_char(&mut app, '界');
    insert_char(&mut app, 'a');
    app.cursor = 1;
    backspace(&mut app);
    assert_eq!(app.input, "a");
}

#[test]
fn renders_the_primary_terminal_regions() {
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    let app = app();
    terminal.draw(|frame| render(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    let contents = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(contents.contains("SCV"));
    assert!(contents.contains("connected"));
    assert!(contents.contains("message"));
    assert!(contents.contains("Enter send"));
}

#[test]
fn running_tools_show_their_latest_progress_line() {
    let screen = |app: &App| {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    let meta = |seq| (String::from("r"), String::from("s"), String::from("t"), seq);
    let mut app = App::new(SessionInfo {
        id: "s".into(),
        cwd: "/tmp".into(),
        model: "test".into(),
        context_max_tokens: 100,
        max_server_frame_bytes: DEFAULT_SERVER_FRAME_LIMIT,
        max_transcript_bytes: 64 * 1024,
        max_transcript_items: 100,
        max_prompt_history_bytes: 1024,
        max_prompt_history_items: 10,
    });
    let (request_id, session_id, turn_id, seq) = meta(1);
    app.handle_server_event(ServerEvent::ToolProposed {
        request_id,
        session_id,
        turn_id,
        seq,
        call_id: "c1".into(),
        name: "agent_codex".into(),
        arguments: serde_json::json!({"prompt":"work"}),
    });
    let (request_id, session_id, turn_id, seq) = meta(2);
    app.handle_server_event(ServerEvent::ToolStarted {
        request_id,
        session_id,
        turn_id,
        seq,
        call_id: "c1".into(),
        name: "agent_codex".into(),
    });
    let (request_id, session_id, turn_id, seq) = meta(3);
    app.handle_server_event(ServerEvent::ToolProgress {
        request_id,
        session_id,
        turn_id,
        seq,
        call_id: "c1".into(),
        text: "$ cargo build\n$ cargo test --workspace".into(),
    });
    let running = screen(&app);
    assert!(running.contains("↳ $ cargo test --workspace"), "{running}");
    assert!(!running.contains("cargo build"));

    let (request_id, session_id, turn_id, seq) = meta(4);
    app.handle_server_event(ServerEvent::ToolCompleted {
        request_id,
        session_id,
        turn_id,
        seq,
        call_id: "c1".into(),
        name: "agent_codex".into(),
        success: true,
        output: "{}".into(),
        truncated: false,
    });
    let finished = screen(&app);
    assert!(!finished.contains("cargo test"), "{finished}");
    // Progress for a tool that is not running is ignored.
    let (request_id, session_id, turn_id, seq) = meta(5);
    app.handle_server_event(ServerEvent::ToolProgress {
        request_id,
        session_id,
        turn_id,
        seq,
        call_id: "c1".into(),
        text: "late line".into(),
    });
    assert!(!screen(&app).contains("late line"));
}

#[tokio::test]
async fn client_reader_rejects_frames_before_unbounded_allocation() {
    let mut reader = BufReader::new(Cursor::new(format!("{}\n", "x".repeat(32))));
    let error = read_bounded_frame(&mut reader, &mut Vec::new(), 8)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exceeded"));
}

#[test]
fn a_server_started_report_turn_is_announced_and_runs_like_any_turn() {
    let mut app = App::new(SessionInfo {
        id: "s".into(),
        cwd: "/tmp".into(),
        model: "test".into(),
        context_max_tokens: 100,
        max_server_frame_bytes: DEFAULT_SERVER_FRAME_LIMIT,
        max_transcript_bytes: 64 * 1024,
        max_transcript_items: 100,
        max_prompt_history_bytes: 1024,
        max_prompt_history_items: 10,
    });
    app.handle_server_event(ServerEvent::TurnStarted {
        request_id: "background:1".into(),
        session_id: "s".into(),
        turn_id: "t2".into(),
        seq: 1,
        origin: Some(scv_protocol::TurnOrigin {
            kind: scv_protocol::ORIGIN_BACKGROUND.into(),
            jobs: vec!["job-1".into()],
        }),
    });
    assert!(app.turn.is_some());
    assert!(app.items.iter().any(|item| matches!(
        item,
        TranscriptItem::System(text) if text.contains("Background work finished (job-1)")
    )));
    app.handle_server_event(ServerEvent::TurnCompleted {
        request_id: "background:1".into(),
        session_id: "s".into(),
        turn_id: "t2".into(),
        seq: 2,
        steps: 1,
        usage: scv_protocol::Usage::default(),
        origin: Some(scv_protocol::TurnOrigin {
            kind: scv_protocol::ORIGIN_BACKGROUND.into(),
            jobs: vec!["job-1".into()],
        }),
    });
    assert!(app.turn.is_none());
}

#[test]
fn server_clear_is_authoritative_for_display_and_prompt_history() {
    let mut app = app();
    app.push_item(TranscriptItem::User("hello".into()));
    app.add_prompt_history("hello".into());
    app.handle_server_event(ServerEvent::SessionCleared {
        request_id: "clear".into(),
        session_id: "s".into(),
        seq: 1,
    });
    assert!(app.items.is_empty());
    assert!(app.prompt_history.is_empty());
    assert_eq!(app.last_seq, 1);
}
