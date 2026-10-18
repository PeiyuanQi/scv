//! Unit tests for `src/process.rs`.

use std::collections::HashMap;

use super::*;

#[test]
fn agent_environment_drops_inherited_credentials_but_keeps_its_own_home() {
    let mut command = std::process::Command::new("true");
    apply_agent_environment_from(
        &mut command,
        [
            "GROK_HOME",
            "XAI_API_KEY",
            "PI_CODING_AGENT_DIR",
            "DEEPSEEK_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "PATH",
        ]
        .map(OsString::from),
        &[("GROK_HOME".into(), "/private/.grok".into())],
    );
    let envs: HashMap<_, _> = command
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
        .collect();
    assert_eq!(
        envs[&OsString::from("GROK_HOME")],
        Some(OsString::from("/private/.grok"))
    );
    for removed in [
        "XAI_API_KEY",
        "PI_CODING_AGENT_DIR",
        "DEEPSEEK_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENROUTER_API_KEY",
    ] {
        assert_eq!(envs[&OsString::from(removed)], None, "{removed}");
    }
    assert!(!envs.contains_key(&OsString::from("PATH")));
}

#[test]
fn process_groups_zero_and_one_are_never_signalled() {
    // Group 0 is this process's own group and 1 is init's; an ID that does
    // not fit a pid_t cannot name a group at all.
    assert_eq!(ProcessGroup::new(0), None);
    assert_eq!(ProcessGroup::new(1), None);
    assert_eq!(ProcessGroup::new(u32::MAX), None);
    assert!(ProcessGroup::new(2).is_some());
}

#[test]
fn unbounded_jobs_keep_bounded_startup_and_approval_deadlines() {
    let now = Instant::now();
    let startup = startup_deadline(None);
    let approval = approval_deadline(None);
    assert!(startup >= now);
    assert!(startup - now <= STARTUP_TIMEOUT + Duration::from_secs(1));
    assert!(approval >= now);
    assert!(approval - now <= APPROVAL_TIMEOUT + Duration::from_secs(1));

    let main = Instant::now() + Duration::from_secs(86_400);
    assert_eq!(startup_deadline(Some(main)), main);
    assert_eq!(approval_deadline(Some(main)), main);
}

fn sh(script: &str, deadline: Option<Instant>) -> ProcessSpec {
    ProcessSpec {
        executable: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        environment: Vec::new(),
        clear_environment: false,
        sanitize_scv_environment: false,
        deadline,
        output_limit: 1024,
    }
}

#[tokio::test]
async fn a_process_without_a_deadline_runs_to_its_end_and_stays_cancellable() {
    let output = execute_process(
        sh("sleep 0.3; echo done", None),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(value["exit_code"], 0, "{value}");
    assert_eq!(value["timed_out"], false);
    assert_eq!(value["output"], "done\n");

    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(200)).await;
        cancel.cancel();
    });
    let error = timeout(
        Duration::from_secs(10),
        execute_process(sh("sleep 30", None), cancellation),
    )
    .await
    .expect("cancellation ends an unbounded process")
    .unwrap_err();
    assert_eq!(error.kind, ToolFailure::Cancelled, "{}", error.message);
}
