//! Unit tests for `src/mail_chat.rs`.

use super::*;
use crate::{Media, MediaKind};

fn owner(text: &str) -> Message {
    Message::text("m1", "owner", text, "ctx", None)
}

fn mail(text: &str) -> MailCommand {
    match command(&owner(text)) {
        Command::Mail(command) => command,
        other => panic!("{text:?} read as {other:?}"),
    }
}

fn invalid(text: &str) -> &'static str {
    match command(&owner(text)) {
        Command::Invalid(why) => why,
        other => panic!("{text:?} read as {other:?}"),
    }
}

#[test]
fn status_and_help() {
    assert_eq!(mail("mail status"), MailCommand::Status);
    assert_eq!(mail("  MAIL   Status。"), MailCommand::Status);
    assert_eq!(command(&owner("mail help")), Command::Help);
    for text in [
        "hello",
        "",
        "status",
        "mail",
        "mail statuses",
        "yes",
        "ok",
        "好",
    ] {
        assert_eq!(command(&owner(text)), Command::Help, "{text:?}");
    }
}

#[test]
fn approve_and_deny_take_codes_in_any_case_with_end_punctuation() {
    assert_eq!(
        mail("approve Q7M2KD"),
        MailCommand::Approve(vec!["Q7M2KD".into()])
    );
    assert_eq!(
        mail("Approve q7m2kd W3H8TN."),
        MailCommand::Approve(vec!["Q7M2KD".into(), "W3H8TN".into()])
    );
    assert_eq!(
        mail("批准 Q7M2KD Q7M2KD！"),
        MailCommand::Approve(vec!["Q7M2KD".into()])
    );
    assert_eq!(mail("deny all"), MailCommand::DenyAll);
    assert_eq!(mail("拒绝 全部"), MailCommand::DenyAll);
    assert_eq!(
        mail("deny W3H8TN!"),
        MailCommand::Deny(vec!["W3H8TN".into()])
    );
    // Codes never contain the confusable letters and digits.
    assert!(invalid("approve Q7M2K0").contains("six letters"));
    assert!(invalid("approve QIM2KD").contains("six letters"));
    assert!(invalid("approve").contains("name the code"));
    assert!(invalid("approve yes").contains("six letters"));
    let many: Vec<String> = (0..21)
        .map(|n| {
            let letters = codes::ALPHABET;
            (0..6)
                .map(|at| char::from(letters[(n * 7 + at) % 30]))
                .collect()
        })
        .collect();
    assert!(invalid(&format!("approve {}", many.join(" "))).contains("at most 20"));
}

#[test]
fn mail_commands_name_handles_addresses_and_free_text() {
    assert_eq!(
        mail("mail reply #4K7P thanks, say Thursday works"),
        MailCommand::Reply {
            handle: "4K7P".into(),
            text: "thanks, say Thursday works".into()
        }
    );
    assert_eq!(
        mail("mail reply ＃4k7p"),
        MailCommand::Reply {
            handle: "4K7P".into(),
            text: String::new()
        }
    );
    assert_eq!(
        mail("mail forward #4K7P bob@example.com,carol@example.org FYI\nsee below"),
        MailCommand::Forward {
            handle: "4K7P".into(),
            to: vec!["bob@example.com".into(), "carol@example.org".into()],
            note: "FYI\nsee below".into()
        }
    );
    assert_eq!(
        mail("mail compose work a@example.com；b@example.com ask about Friday"),
        MailCommand::Compose {
            account: Some("work".into()),
            to: vec!["a@example.com".into(), "b@example.com".into()],
            text: "ask about Friday".into()
        }
    );
    // Slack links the addresses it is sent.
    assert_eq!(
        mail(
            "mail forward #4K7P <mailto:bob@example.com|bob@example.com>,<mailto:carol@example.org> FYI"
        ),
        MailCommand::Forward {
            handle: "4K7P".into(),
            to: vec!["bob@example.com".into(), "carol@example.org".into()],
            note: "FYI".into()
        }
    );
    assert_eq!(
        mail("mail compose <mailto:eve@example.net|bob@example.com> hi"),
        MailCommand::Compose {
            account: None,
            to: vec!["<mailto:eve@example.net|bob@example.com>".into()],
            text: "hi".into()
        },
        "a link that shows another address is kept, to be refused as an address"
    );
    assert_eq!(
        mail("mail compose a@example.com hi"),
        MailCommand::Compose {
            account: None,
            to: vec!["a@example.com".into()],
            text: "hi".into()
        }
    );
    assert_eq!(
        mail("mail revise Q7M2KD shorter please"),
        MailCommand::Revise {
            code: "Q7M2KD".into(),
            text: "shorter please".into()
        }
    );
    for (text, action) in [
        ("mail archive #4K7P", MessageAction::Archive),
        ("mail read 4K7P.", MessageAction::Read),
        ("mail trash #4K7P", MessageAction::Trash),
        ("mail spam #4K7P", MessageAction::Spam),
        ("mail junk #4K7P", MessageAction::Spam),
    ] {
        assert_eq!(
            mail(text),
            MailCommand::Message {
                handle: "4K7P".into(),
                action
            },
            "{text}"
        );
    }
}

#[test]
fn malformed_commands_say_why_and_do_nothing() {
    assert!(invalid("mail reply").contains("which mail"));
    assert!(invalid("mail trash #4K7P now").contains("which mail"));
    assert!(invalid("mail forward #4K7P not-an-address").contains("addresses"));
    assert!(invalid("mail compose a@example.com").contains("what to write"));
    assert!(invalid("mail revise Q7M2KD").contains("what to change"));
    let long = format!("mail reply #4K7P {}", "x".repeat(MAX_TEXT_BYTES + 1));
    assert!(invalid(&long).contains("2 KiB"));
    let eleven: Vec<String> = (0..11).map(|n| format!("a{n}@example.com")).collect();
    assert!(invalid(&format!("mail compose {} hi", eleven.join(","))).contains("addresses"));
}

#[test]
fn quotes_forwards_and_files_are_never_commands() {
    let mut quoted = owner("approve Q7M2KD");
    quoted.quoted = true;
    assert_eq!(command(&quoted), Command::Help);
    let mut referenced = owner("mail status");
    referenced.reference = Some("{\"parent\":\"om_1\"}".into());
    assert_eq!(command(&referenced), Command::Help);
    let mut file = owner("approve Q7M2KD");
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
    assert_eq!(status(&[], 0), NO_ACCOUNTS_REPLY);
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
        ..MailCounts::default()
    };
    let text = status(
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
