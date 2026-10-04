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
}
