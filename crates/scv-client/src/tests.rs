//! Unit tests for `src/lib.rs`.

use super::*;

#[test]
fn only_a_positive_inherited_depth_is_declared() {
    assert_eq!(parse_delegation_depth(Some("2")), Some(2));
    assert_eq!(parse_delegation_depth(Some(" 1\n")), Some(1));
    assert_eq!(parse_delegation_depth(Some("0")), None);
    assert_eq!(parse_delegation_depth(Some("deep")), None);
    assert_eq!(parse_delegation_depth(None), None);
}

#[test]
fn a_command_that_can_stop_an_account_outlasts_a_mail_actions_grace() {
    for command in [
        DaemonCommand::Reload,
        DaemonCommand::ChannelLogout {
            channel: "email".into(),
            account: "default".into(),
        },
    ] {
        assert!(control_timeout(&command) > Duration::from_secs(65));
    }
    assert_eq!(
        control_timeout(&DaemonCommand::Status),
        Duration::from_secs(20)
    );
    // Steering waits up to 30 seconds, so a send needs the longer budget.
    assert_eq!(
        control_timeout(&DaemonCommand::WorkbenchSend {
            session: "s".into(),
            conversation: "codex-1".into(),
            prompt: "hi".into(),
            mode: scv_protocol::SendMode::Auto,
        }),
        Duration::from_secs(90)
    );
    assert_eq!(
        control_timeout(&DaemonCommand::Workbench { watch: true }),
        Duration::from_secs(20)
    );
    // Opening an agent's terminal may first stop its server and run the
    // project Rust preflight; typing into it never waits that long.
    assert_eq!(
        control_timeout(&DaemonCommand::Attach {
            session: "s".into(),
            conversation: "codex-1".into(),
            cols: None,
            rows: None,
        }),
        Duration::from_secs(90)
    );
    assert_eq!(
        control_timeout(&DaemonCommand::AttachIo {
            id: "at-1".into(),
            after: 0,
            input: String::new(),
            cols: None,
            rows: None,
        }),
        Duration::from_secs(20)
    );
}
