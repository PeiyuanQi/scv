//! Unit tests for `src/mail_chat.rs`.

use super::*;
use crate::{Media, MediaKind};

fn owner(text: &str) -> Message {
    Message::text("m1", "owner", text, "ctx", None)
}

#[test]
fn status_and_help_are_the_only_commands() {
    assert_eq!(command(&owner("mail status")), Command::Status);
    assert_eq!(command(&owner("  MAIL   Status。")), Command::Status);
    assert_eq!(command(&owner("mail help")), Command::Help);
    for text in [
        "hello",
        "",
        "status",
        "mail",
        "mail statuses",
        "approve",
        "yes",
    ] {
        assert_eq!(command(&owner(text)), Command::Help, "{text:?}");
    }
}

#[test]
fn action_commands_are_recognised_only_to_refuse_them() {
    for text in [
        "approve Q7M2KD",
        "Approve q7m2kd W3H8TN.",
        "批准 Q7M2KD",
        "deny all",
        "拒绝 Q7M2KD！",
        "mail reply #4K7P thanks",
        "mail trash #4K7P",
        "mail spam 4K7P",
        "mail compose a@example.com hi",
        "mail revise Q7M2KD shorter",
    ] {
        assert_eq!(command(&owner(text)), Command::Action, "{text:?}");
        assert_eq!(reply(command(&owner(text)), &[], 0), LATER_REPLY);
    }
}

#[test]
fn quotes_forwards_and_files_are_never_commands() {
    let mut quoted = owner("approve Q7M2KD");
    quoted.quoted = true;
    assert_eq!(command(&quoted), Command::Help);
    let mut referenced = owner("mail status");
    referenced.reference = Some("{\"parent\":\"om_1\"}".into());
    assert_eq!(command(&referenced), Command::Help);
    let mut file = owner("mail status");
    file.media.push(Media {
        kind: MediaKind::Image,
        name: String::new(),
        size: None,
        mime: None,
        transcript: None,
        source: "x".into(),
    });
    assert_eq!(command(&file), Command::Help);
}

#[test]
fn status_shows_counts_only() {
    assert_eq!(reply(Command::Status, &[], 0), NO_ACCOUNTS_REPLY);
    let counts = MailCounts {
        claimed: 1,
        queued: 2,
        seen_today: 9,
        triaged_today: 4,
        reported_today: 3,
        tokens_today: 1234,
        token_budget: 150_000,
        messages_24h: 5,
        last_check_unix_seconds: Some(1_000),
    };
    let text = reply(
        Command::Status,
        &[
            ("email:default".into(), counts.clone()),
            (
                "email:work".into(),
                MailCounts {
                    token_budget: 0,
                    last_check_unix_seconds: None,
                    ..counts
                },
            ),
        ],
        1_000 + 125,
    );
    assert_eq!(
        text,
        "email:default: 9 new today, 4 triaged, 3 reported; 2 waiting to be sent; 1234 of \
         150000 model tokens used; last check 2 min ago.\n\
         email:work: 9 new today, 4 triaged, 3 reported; 2 waiting to be sent; the model is \
         off; not checked yet."
    );
}
