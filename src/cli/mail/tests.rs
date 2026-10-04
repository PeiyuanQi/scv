//! Unit tests for `src/cli/mail.rs`.

use super::*;
use scv_protocol::{ComponentState, MailCounts, RemoteTools};

fn health(mail: Option<MailCounts>) -> ComponentHealth {
    ComponentHealth {
        id: "email:default".into(),
        channel: "email".into(),
        account: "default".into(),
        bot_id: Some("imap.example.com".into()),
        user_id: None,
        enabled: true,
        state: ComponentState::Connected,
        last_success_unix_seconds: None,
        error: None,
        restarts: 0,
        remote_tools: RemoteTools::None,
        senders: None,
        purpose: None,
        mail,
    }
}

#[test]
fn an_account_line_says_what_it_may_do_as_counts() {
    assert_eq!(account_line(&health(None)), "email:default: not running");
    assert_eq!(
        account_line(&health(Some(MailCounts::default()))),
        "email:default (imap): reads mail only"
    );
    let counts = MailCounts {
        provider: Some("gmail".into()),
        actions: vec!["draft".into(), "trash".into()],
        actions_open: 2,
        actions_executing: 1,
        ..MailCounts::default()
    };
    let line = account_line(&health(Some(counts)));
    assert!(
        line.starts_with("email:default (gmail): may draft, trash once you approve each"),
        "{line}"
    );
    assert!(line.contains("2 waiting, 1 being carried out"), "{line}");
}

#[test]
fn an_action_line_names_it_by_id_kind_and_state_only() {
    let action = MailAction {
        account: "email:default".into(),
        id: format!("a{}", "0".repeat(32)),
        kind: "send".into(),
        state: "open".into(),
        created_unix_seconds: 100,
        expires_unix_seconds: Some(100 + 3 * 3600 + 5 * 60),
    };
    assert_eq!(
        action_line(&action, 100),
        format!("a{} send open, 3 h 5 min left to approve", "0".repeat(32))
    );
    let approved = MailAction {
        state: "approved".into(),
        expires_unix_seconds: None,
        ..action
    };
    assert!(action_line(&approved, 100).ends_with("send approved"));
}
