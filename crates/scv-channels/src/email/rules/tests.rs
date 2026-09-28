//! Unit tests for `src/email/rules.rs`.

use super::*;
use crate::email::source::{Address, Signals, SourceRef};

fn meta(from: &str) -> Meta {
    Meta {
        source: SourceRef::Imap {
            mailbox: "INBOX".into(),
            uidvalidity: 1,
            uid: 1,
        },
        identity: "id".into(),
        received_at: 0,
        size: 10,
        from: Some(Address {
            name: String::new(),
            address: from.into(),
        }),
        reply_to: None,
        to: Vec::new(),
        cc: Vec::new(),
        subject: String::new(),
        message_id: None,
        signals: Signals::default(),
        category: None,
        text: None,
        attachments: Vec::new(),
    }
}

fn rule(action: RuleAction) -> Rule {
    Rule {
        from: Vec::new(),
        list_id: Vec::new(),
        bulk: None,
        noreply: None,
        action,
        urgent: false,
    }
}

#[test]
fn built_in_rules_count_bulk_report_noreply_and_triage_the_rest() {
    assert_eq!(
        decide(&[], &meta("alice@example.com")).action,
        RuleAction::Triage
    );
    let mut list = meta("news@example.com");
    list.signals.list_id = Some("News <news.example.com>".into());
    assert_eq!(decide(&[], &list).action, RuleAction::Count);
    let mut automated = meta("robot@example.com");
    automated.signals.auto_submitted = Some("auto-generated".into());
    assert_eq!(decide(&[], &automated).action, RuleAction::Count);
    let mut human = meta("robot@example.com");
    human.signals.auto_submitted = Some("no".into());
    assert_eq!(decide(&[], &human).action, RuleAction::Triage);
    for sender in [
        "noreply@example.com",
        "No-Reply@example.com",
        "no-reply+abc@example.com",
        "donotreply@example.com",
        "MAILER-DAEMON@example.com",
    ] {
        assert_eq!(
            decide(&[], &meta(sender)).action,
            RuleAction::Header,
            "{sender}"
        );
    }
    assert_eq!(
        decide(&[], &meta("noreplyer@example.com")).action,
        RuleAction::Triage
    );
    let mut bounce = meta("someone@example.com");
    bounce.signals.null_return_path = true;
    assert_eq!(decide(&[], &bounce).action, RuleAction::Header);
    let mut nobody = meta("");
    nobody.from = None;
    assert_eq!(decide(&[], &nobody).action, RuleAction::Triage);
}

#[test]
fn the_first_matching_owner_rule_wins_over_the_built_in_ones() {
    let urgent_boss = Rule {
        from: vec!["boss@example.com".into()],
        urgent: true,
        ..rule(RuleAction::Triage)
    };
    let bank = Rule {
        from: vec!["@bank.example".into()],
        ..rule(RuleAction::Header)
    };
    let newsletter = Rule {
        list_id: vec!["news.example.com".into()],
        ..rule(RuleAction::Triage)
    };
    let rules = [urgent_boss, bank, newsletter];
    assert_eq!(
        decide(&rules, &meta("Boss@Example.com")),
        Decision {
            action: RuleAction::Triage,
            urgent: true
        }
    );
    assert_eq!(
        decide(&rules, &meta("alerts@bank.example")).action,
        RuleAction::Header
    );
    assert_eq!(
        decide(&rules, &meta("alerts@mail.bank.example")).action,
        RuleAction::Header
    );
    // A look-alike domain is not a subdomain.
    assert_eq!(
        decide(&rules, &meta("alerts@evilbank.example")).action,
        RuleAction::Triage
    );
    let mut list = meta("news@example.com");
    list.signals.list_id = Some("Weekly <NEWS.example.com>".into());
    assert_eq!(decide(&rules, &list).action, RuleAction::Triage);
}

#[test]
fn a_rule_holds_only_when_every_condition_it_names_holds() {
    let quiet_bulk = Rule {
        from: vec!["@example.com".into()],
        bulk: Some(true),
        ..rule(RuleAction::Header)
    };
    let mut list = meta("news@example.com");
    list.signals.precedence = Some("bulk".into());
    assert_eq!(
        decide(std::slice::from_ref(&quiet_bulk), &list).action,
        RuleAction::Header
    );
    // Not bulk: the rule does not hold, and the built-ins triage it.
    assert_eq!(
        decide(&[quiet_bulk], &meta("person@example.com")).action,
        RuleAction::Triage
    );
    // A rule without conditions matches everything.
    assert_eq!(
        decide(&[rule(RuleAction::Count)], &meta("a@b.example")).action,
        RuleAction::Count
    );
    let humans = Rule {
        noreply: Some(false),
        ..rule(RuleAction::TriageMeta)
    };
    assert_eq!(
        decide(std::slice::from_ref(&humans), &meta("a@b.example")).action,
        RuleAction::TriageMeta
    );
    assert_eq!(
        decide(&[humans], &meta("noreply@b.example")).action,
        RuleAction::Header
    );
}
