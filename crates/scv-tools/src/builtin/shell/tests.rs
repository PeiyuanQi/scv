//! Unit tests for `src/builtin/shell.rs`.

use super::*;

/// `bash -l` sources the host's login profile before it runs a command,
/// and CI images can spend seconds there under parallel test load. Waits
/// that include shell startup use this ceiling; they end as soon as their
/// condition holds.
const SHELL_STARTUP: Duration = Duration::from_secs(30);

/// Polls `probe` every 10 ms until it yields a value or `limit` passes.
async fn wait_for<T>(limit: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn is_gone(pid: i32) -> Option<()> {
    // SAFETY: signal 0 only checks that the process exists.
    (unsafe { libc::kill(pid, 0) } != 0).then_some(())
}

#[tokio::test]
async fn bash_timeout_terminates_the_process() {
    let workspace = tempfile::tempdir().unwrap();
    let tool = BashTool {
        timeout: Duration::from_millis(50),
        max_timeout: Duration::from_millis(50),
        output_limit: 100,
    };
    let started = std::time::Instant::now();
    let output = tool
        .execute(
            json!({"command":"sleep 5"}),
            ToolContext::new(
                workspace.path().canonicalize().unwrap(),
                tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(output.failure, Some(scv_core::ToolFailure::Limit));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn bash_output_is_bounded_and_reports_truncation() {
    let workspace = tempfile::tempdir().unwrap();
    let tool = BashTool {
        timeout: SHELL_STARTUP,
        max_timeout: SHELL_STARTUP,
        output_limit: 8,
    };
    let output = tool
        .execute(
            json!({"command":"printf 12345678901234567890"}),
            ToolContext::new(
                workspace.path().canonicalize().unwrap(),
                tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await
        .unwrap();
    assert!(output.truncated);
    assert!(output.content.contains("12345678"));
    assert!(!output.content.contains("123456789"));
}

#[tokio::test]
async fn bash_cancellation_terminates_the_process_group() {
    let workspace = tempfile::tempdir().unwrap();
    let tool = BashTool {
        timeout: Duration::from_secs(30),
        max_timeout: Duration::from_secs(30),
        output_limit: 100,
    };
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancel = cancellation.clone();
    let started = std::time::Instant::now();
    let execution = tokio::spawn(async move {
        tool.execute(
            json!({"command":"sleep 30"}),
            ToolContext::new(workspace.path().canonicalize().unwrap(), cancellation),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let error = execution.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(error.kind, scv_core::ToolFailure::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn background_descendant_cannot_hold_output_pipes_open() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let tool = BashTool {
        timeout: SHELL_STARTUP,
        max_timeout: SHELL_STARTUP,
        output_limit: 100,
    };
    let output = tool
        .execute(
            json!({"command":"sleep 60 & echo $! > background.pid; exit 0"}),
            ToolContext::new(root.clone(), tokio_util::sync::CancellationToken::new()),
        )
        .await
        .unwrap();
    let returned = std::time::SystemTime::now();
    assert!(!output.is_error());
    // Time from the shell's last write, which excludes its startup.
    let exited = std::fs::metadata(root.join("background.pid"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(returned.duration_since(exited).unwrap_or_default() < Duration::from_secs(3));
    let pid: i32 = std::fs::read_to_string(root.join("background.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        wait_for(Duration::from_secs(5), || is_gone(pid))
            .await
            .is_some(),
        "background descendant {pid} survived tool completion"
    );
}

#[tokio::test]
async fn cancellation_kills_a_term_ignoring_descendant() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let tool = BashTool {
        timeout: Duration::from_secs(30),
        max_timeout: Duration::from_secs(30),
        output_limit: 100,
    };
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancel = cancellation.clone();
    let command_root = root.clone();
    let execution = tokio::spawn(async move {
        tool.execute(
                json!({"command":"trap '' TERM; (trap '' TERM; sleep 30) & echo $! > stubborn.pid; wait"}),
                ToolContext::new(command_root, cancellation),
            )
            .await
    });
    let pid_path = root.join("stubborn.pid");
    let pid = wait_for(SHELL_STARTUP, || {
        std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|value| value.trim().parse::<i32>().ok())
    })
    .await
    .expect("command did not report its descendant pid");
    let started = std::time::Instant::now();
    cancel.cancel();
    let error = execution.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        wait_for(Duration::from_secs(5), || is_gone(pid))
            .await
            .is_some(),
        "TERM-ignoring descendant {pid} survived cancellation"
    );
}
