//! Unit tests for `src/args.rs`.

use std::time::Duration;

use super::*;

#[test]
fn no_timeout_requires_a_background_job_and_is_exclusive() {
    let timeouts = Timeouts {
        default: Duration::from_secs(60),
        max: Duration::from_secs(600),
    };
    let foreground = timeouts.deadline(None, true, false).unwrap_err().message;
    assert!(
        foreground.contains("only allowed for background"),
        "{foreground}"
    );
    let combined = timeouts.deadline(Some(10), true, true).unwrap_err().message;
    assert!(combined.contains("cannot be combined"), "{combined}");
    assert!(timeouts.deadline(None, true, true).unwrap().is_none());
}

#[test]
fn ordinary_timeout_stays_bounded() {
    let timeouts = Timeouts {
        default: Duration::from_secs(60),
        max: Duration::from_secs(600),
    };
    let deadline = timeouts.deadline(Some(10), false, false).unwrap();
    assert!(deadline.is_some());
}
