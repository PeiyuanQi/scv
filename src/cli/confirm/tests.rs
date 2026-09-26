//! Unit tests for `src/cli/confirm.rs`.

use super::*;

#[test]
fn only_yes_exits_zero_and_only_a_known_no_exits_one() {
    assert_eq!(exit_code(ConfirmState::Pending), None);
    assert_eq!(exit_code(ConfirmState::Yes), Some(0));
    assert_eq!(exit_code(ConfirmState::No), Some(1));
    assert_eq!(exit_code(ConfirmState::Expired), Some(1));
    for state in [
        ConfirmState::Withdrawn,
        ConfirmState::Failed,
        ConfirmState::Unknown,
    ] {
        assert_eq!(exit_code(state), Some(2), "{state:?}");
    }
}

#[test]
fn a_daemon_too_old_or_missing_is_named() {
    let old = anyhow::Error::from(ControlError::Server {
        code: ErrorCode::InvalidJson,
        message: "unknown variant `confirm_ask`".into(),
    });
    assert!(not_asked(&old).contains("too old"), "{}", not_asked(&old));
    let missing = anyhow::Error::from(ControlError::Unavailable(std::io::Error::from(
        std::io::ErrorKind::NotFound,
    )));
    assert!(not_asked(&missing).contains("no SCV daemon"));
    let refused = anyhow::Error::from(ControlError::Server {
        code: ErrorCode::ConfirmError,
        message: "no owner chat to ask in".into(),
    });
    assert_eq!(not_asked(&refused), "no owner chat to ask in");
}

#[test]
fn waits_are_told_in_minutes() {
    assert_eq!(minutes(1800), "30 minutes");
    assert_eq!(minutes(1), "1 minute");
    assert_eq!(minutes(90), "2 minutes");
}
