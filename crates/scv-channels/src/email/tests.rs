//! Unit tests for `src/email/mod.rs`: the pieces together, and where mail
//! text may and may not end up.

use super::*;
use crate::email::ledger::Ledger;
use crate::email::source::AttachmentInfo;
use crate::email::test_support::{FakeMailbox, FixedClock, daemon, ledger, mail_chat, meta};
use crate::hub::Hub;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixListener;

mod actions;
mod oauth_login;

#[tokio::test]
async fn a_sibling_exit_lets_the_executor_finish_before_it_is_dropped() {
    let stop = tokio_util::sync::CancellationToken::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started_executor = Arc::clone(&started);
    let dropped_executor = Arc::clone(&dropped);
    let stop_executor = stop.clone();
    let executor = async move {
        struct Guard {
            dropped: Arc<std::sync::atomic::AtomicBool>,
            done: bool,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                if !self.done {
                    self.dropped
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        let mut guard = Guard {
            dropped: dropped_executor,
            done: false,
        };
        started_executor.notify_one();
        stop_executor.cancelled().await;
        guard.done = true;
        Ok(())
    };
    let started_others = Arc::clone(&started);
    let others = async move {
        started_others.notified().await;
        Err(anyhow::anyhow!("the worker stopped"))
    };
    let error = beside_executor(others, executor, &stop, STOP_GRACE)
        .await
        .expect_err("the sibling's error is returned after the executor finishes");
    assert!(error.to_string().contains("worker stopped"), "{error:#}");
    assert!(
        !dropped.load(std::sync::atomic::Ordering::SeqCst),
        "the executor was dropped before it finished"
    );
}

#[tokio::test]
async fn the_executor_finishing_stops_the_other_tasks() {
    let stop = tokio_util::sync::CancellationToken::new();
    let executor = async { Ok(()) };
    let others = std::future::pending();
    beside_executor(others, executor, &stop, STOP_GRACE)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_sibling_exit_waits_only_up_to_the_bound() {
    let stop = tokio_util::sync::CancellationToken::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let started_executor = Arc::clone(&started);
    let executor = async move {
        started_executor.notify_one();
        std::future::pending().await
    };
    let started_others = Arc::clone(&started);
    let others = async move {
        started_others.notified().await;
        Err(anyhow::anyhow!("the janitor stopped"))
    };
    let started_at = std::time::Instant::now();
    let error = beside_executor(others, executor, &stop, Duration::from_millis(50))
        .await
        .expect_err("the bound ends the wait");
    assert!(
        error.to_string().contains("did not finish in time"),
        "{error:#}"
    );
    assert!(started_at.elapsed() < Duration::from_secs(2));
}

const CANARY: &str = "CANARY-7F3Q";

/// Every tracing event, as text.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every file under `root` whose content holds `needle`.
fn files_holding(root: &std::path::Path, needle: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).unwrap().flatten() {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file()
                && String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains(needle)
            {
                found.push(path);
            }
        }
    }
    found
}

fn settings() -> MailSettings {
    MailSettings::parse(Some(
        &"[notify]\nroute = [\"fake:mail\"]\nsettle_seconds = 0\n"
            .parse()
            .unwrap(),
    ))
    .unwrap()
}

#[tokio::test]
async fn mail_text_reaches_the_mail_chat_and_nothing_else() {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _logs = tracing::subscriber::set_default(subscriber);

    let home = tempfile::tempdir().unwrap();
    let ledger: Ledger = ledger(home.path());
    let settings = settings();
    let clock = FixedClock::new();
    let low_space = LowSpace::default();
    let socket = home.path().join("daemon.sock");
    let (seen, _daemon) = daemon(UnixListener::bind(&socket).unwrap(), |_| {
        format!("{{\"notify\": true, \"urgent\": false, \"summary\": [\"{CANARY} summary\"]}}")
    });
    let hub = Hub::new(None);
    let (texts, _chat) = mail_chat(&hub, "fake:mail", crate::state::Purpose::Mail);
    let private = home.path().join("state/mail/default");
    let cwd = private.join("empty");
    state::private_directory(home.path(), &cwd).unwrap();
    let worker = worker::Worker {
        ledger: &ledger,
        settings: &settings,
        socket: &socket,
        cwd: &cwd,
        clock: &clock,
        low_space: &low_space,
        frame: triage::frame("default", &settings.instructions),
        options: triage::Options::default(),
        actions: None,
    };
    let notifier = notify::Notifier {
        ledger: &ledger,
        settings: &settings,
        account: "default",
        clock: &clock,
        hub: Some(&hub),
        previews: None,
    };
    let mut mailbox = FakeMailbox::default();
    assert!(worker.check(&mut mailbox).await.is_ok());
    let mut mail = meta(1, "alice@example.com", &format!("{CANARY} subject"));
    mail.from.as_mut().unwrap().name = format!("{CANARY} name");
    mail.attachments.push(AttachmentInfo {
        name: format!("{CANARY}.pdf"),
        mime: "application/pdf".into(),
        size: 10,
    });
    mailbox.add(mail, &format!("{CANARY} body text"));
    assert!(worker.check(&mut mailbox).await.is_ok());

    // The tool-free session saw the mail, framed; its prompt held nothing
    // of the owner's configuration.
    let sessions = seen.lock().unwrap().clone();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].prompt.contains(&format!("{CANARY} body text")));
    let start = sessions[0].start.as_object().unwrap().clone();
    assert_eq!(start["no_tools"], true);
    assert!(!start.contains_key("channel"));
    assert!(!start["system_prompt"].as_str().unwrap().contains(CANARY));

    // Before delivery the report waits in the bounded queue, the one place
    // at rest that may hold it.
    let waiting = files_holding(home.path(), CANARY);
    assert_eq!(
        waiting,
        [home.path().join("state/channels/email/default.json")]
    );

    notifier.step().await.unwrap();
    let delivered = texts.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    let digest = &delivered[0].1;
    for field in ["subject", "name", "summary"] {
        assert!(
            digest.contains(&format!("│ {CANARY} {field}"))
                || digest.contains(&format!(": {CANARY} {field}")),
            "{digest}"
        );
    }
    assert!(
        digest.contains(&format!("│ Attachment: {CANARY}.pdf")),
        "{digest}"
    );
    assert!(
        !digest.contains("body text"),
        "the body itself is never reported"
    );
    notifier.step().await.unwrap();

    // Once delivered, nothing at rest holds it, and no log line or status
    // count ever did.
    assert!(files_holding(home.path(), CANARY).is_empty());
    let logs = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    assert!(!logs.is_empty());
    assert!(!logs.contains(CANARY), "{logs}");
    assert!(!logs.contains("alice@example.com"), "{logs}");
    let status = serde_json::to_string(&counts(
        &ledger,
        &settings,
        &clock,
        source::ProviderKind::Imap,
    ))
    .unwrap();
    assert!(!status.contains(CANARY), "{status}");
    assert!(status.contains("\"reported_today\":1"), "{status}");
}

#[tokio::test(start_paused = true)]
async fn a_refused_sign_in_logs_nothing_the_server_said_or_was_sent() {
    const SECRET: &str = "hunter2-SECRET";
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _logs = tracing::subscriber::set_default(subscriber);

    let home = tempfile::tempdir().unwrap();
    let ledger: Ledger = ledger(home.path());
    let settings = settings();
    let clock = FixedClock::new();
    let low_space = LowSpace::default();
    let worker = worker::Worker {
        ledger: &ledger,
        settings: &settings,
        socket: &home.path().join("daemon.sock"),
        cwd: home.path(),
        clock: &clock,
        low_space: &low_space,
        frame: triage::frame("default", ""),
        options: triage::Options::default(),
        actions: None,
    };
    let config = imap::ImapConfig {
        host: "imap.example.com".into(),
        port: 993,
        username: "me@example.com".into(),
        password: SECRET.into(),
        mailbox: "INBOX".into(),
    };
    // The server echoes the password, and fakes a second log line.
    let connect = || async {
        let (stream, _server) = imap::fake::serve(
            "* OK [CAPABILITY IMAP4rev1] ready\r\n",
            vec![imap::fake::command(
                &format!("LOGIN \"me@example.com\" \"{SECRET}\""),
                &format!(
                    "{{tag}} NO [AUTHENTICATIONFAILED] wrong password {SECRET}\u{2028}INFO ok\r\n"
                ),
            )],
        );
        imap::ImapSource::start(stream, &config).await
    };
    let healthy = Mutex::new(Vec::new());
    let report = |ok| healthy.lock().unwrap().push(ok);
    let watched =
        tokio::time::timeout(Duration::from_secs(120), worker.watch(connect, &report)).await;
    assert!(watched.is_err(), "a refused sign-in is retried, not fatal");
    assert!(!healthy.lock().unwrap().is_empty());
    assert!(healthy.lock().unwrap().iter().all(|ok| !ok));

    let logs = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    assert!(logs.contains("could not open the mailbox"), "{logs}");
    assert!(logs.contains("[AUTHENTICATIONFAILED]"), "{logs}");
    for leaked in [
        SECRET,
        "hunter2",
        "me@example.com",
        "wrong password",
        "INFO ok",
    ] {
        assert!(!logs.contains(leaked), "{leaked}: {logs}");
    }
    assert!(!logs.contains('\u{2028}'), "{logs}");
}

#[tokio::test]
async fn every_file_is_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    let ledger = ledger(home.path());
    ledger
        .note("system:x:d", plan::Class::System, "x".into(), 1)
        .await
        .unwrap();
    state::private_directory(home.path(), &home.path().join("state/mail/default/empty")).unwrap();
    for root in ["state/channels/email", "state/mail", "credentials/email"] {
        let mut stack = vec![home.path().join(root)];
        while let Some(path) = stack.pop() {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let mode = metadata.permissions().mode() & 0o777;
            if metadata.is_dir() {
                assert_eq!(mode, 0o700, "{}", path.display());
                stack.extend(
                    std::fs::read_dir(&path)
                        .unwrap()
                        .flatten()
                        .map(|e| e.path()),
                );
            } else {
                assert_eq!(mode, 0o600, "{}", path.display());
            }
        }
    }
}

#[tokio::test]
async fn logout_removes_the_state_and_the_private_directory() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    let ledger = ledger(home.path());
    ledger
        .note("system:x:d", plan::Class::System, "x".into(), 1)
        .await
        .unwrap();
    drop(ledger);
    let empty = layout.mail_state("default").join("empty");
    state::private_directory(home.path(), &empty).unwrap();
    std::fs::write(empty.join("stray"), "x").unwrap();
    let accounts = ChannelKind::Email.accounts(&layout);
    assert_eq!(accounts.names().unwrap(), ["default"]);
    accounts.remove("default").unwrap();
    assert!(!layout.mail_state("default").exists());
    assert!(
        !home
            .path()
            .join("state/channels/email/default.json")
            .exists()
    );
    assert!(!home.path().join("credentials/email/default.json").exists());
}

#[test]
fn status_shows_the_server_and_never_the_user_name() {
    let credentials = test_support::account();
    assert_eq!(Email::owner(&credentials), None);
    assert_eq!(
        Email::bot_id(&credentials).as_deref(),
        Some("imap.example.com")
    );
}

#[test]
fn every_route_must_name_a_chat() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    for (route, expected) in [
        ("email:other", "not a chat"),
        ("irc:mail", "not a chat"),
        ("nothing", "not a chat"),
    ] {
        let error = check_routes(&layout, &[route.into()]).unwrap_err();
        assert!(error.to_string().contains(expected), "{route}: {error}");
    }
}

#[cfg(feature = "feishu")]
#[test]
fn every_route_must_be_a_configured_mail_chat() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    state::atomic_write(
        &layout.config(),
        "[channels.feishu.mail]\npurpose = \"mail\"\n[channels.feishu.chat]\nenabled = true\n",
    )
    .unwrap();
    assert!(check_routes(&layout, &["feishu:mail".into()]).is_ok());
    for (route, expected) in [
        ("feishu:chat", "not a mail chat"),
        ("feishu:missing", "not a mail chat"),
        ("email:other", "not a chat"),
        ("irc:mail", "not a chat"),
    ] {
        let error = check_routes(&layout, &["feishu:mail".into(), route.into()]).unwrap_err();
        assert!(error.to_string().contains(expected), "{route}: {error}");
    }
}
