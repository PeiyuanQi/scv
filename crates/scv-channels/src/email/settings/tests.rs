//! Unit tests for `src/email/settings.rs`.

use super::*;

fn parse(text: &str) -> Result<MailSettings> {
    let table: toml::Table = text.parse().unwrap();
    MailSettings::parse(Some(&table))
}

const ROUTE: &str = "[notify]\nroute = [\"feishu:mail\"]\n";

#[test]
fn defaults_need_only_a_route() {
    let error = MailSettings::parse(None).unwrap_err();
    assert!(error.to_string().contains("mail.notify.route"), "{error}");
    let settings = parse(ROUTE).unwrap();
    assert_eq!(settings.mailbox, "INBOX");
    assert_eq!(settings.max_body_kib, 8);
    assert_eq!(settings.max_fetch_kib, 64);
    assert_eq!(settings.max_tokens_per_day, 150_000);
    assert_eq!(settings.catchup_hours, 24);
    assert!(settings.send_body);
    assert_eq!(settings.notify.settle_seconds, 120);
    assert_eq!(settings.notify.max_delay_seconds, 900);
    assert_eq!(settings.notify.quiet().unwrap(), None);
    assert_eq!(settings.notify.fixed_offset().unwrap(), None);
    assert_eq!(settings.retention.max_state_kib, 2048);
}

#[test]
fn mail_actions_are_off_until_each_kind_is_set_to_approve() {
    assert_eq!(parse(ROUTE).unwrap().actions, None);
    let empty = parse(&format!("{ROUTE}[actions]\n"))
        .unwrap()
        .actions
        .unwrap();
    assert_eq!(empty, ActionSettings::default());
    assert!(!empty.any());
    assert!(!empty.changes_mailbox());
    // Forwarding is a form of drafting or sending: alone it does nothing.
    let forward = parse(&format!("{ROUTE}[actions]\nforward = \"approve\"\n"))
        .unwrap()
        .actions
        .unwrap();
    assert!(!forward.any());
    let send = parse(&format!("{ROUTE}[actions]\nsend = \"approve\"\n"))
        .unwrap()
        .actions
        .unwrap();
    assert!(send.any());
    assert!(!send.changes_mailbox(), "sending alone writes no folder");
    for kind in ["draft", "archive", "mark_read", "trash", "spam"] {
        let actions = parse(&format!("{ROUTE}[actions]\n{kind} = \"approve\"\n"))
            .unwrap()
            .actions
            .unwrap();
        assert!(actions.any() && actions.changes_mailbox(), "{kind}");
    }
    // No value carries an action out without its approval.
    for value in ["\"on\"", "\"auto\"", "\"always\"", "true", "1"] {
        assert!(
            parse(&format!("{ROUTE}[actions]\nsend = {value}\n")).is_err(),
            "{value}"
        );
    }
}

#[test]
fn mail_action_limits_and_names_are_checked() {
    for (extra, expected) in [
        ("typo = 1", "unknown field"),
        ("delete = \"approve\"", "unknown field"),
        ("max_recipients = 0", "max_recipients"),
        ("max_recipients = 11", "max_recipients"),
        ("approval_hours = 0", "approval_hours"),
        ("approval_hours = 169", "approval_hours"),
        (
            "approval_hours = 48\nmax_pending_hours = 24",
            "max_pending_hours",
        ),
        ("max_pending_hours = 337", "max_pending_hours"),
        ("execute_minutes = 0", "execute_minutes"),
        ("execute_minutes = 121", "execute_minutes"),
        ("max_open = 0", "max_open"),
        ("max_open = 129", "max_open"),
        ("max_sends_per_day = 501", "max_sends_per_day"),
        ("max_drafts_per_day = 1001", "max_drafts_per_day"),
        ("max_moves_per_day = 1001", "max_moves_per_day"),
        ("max_flags_per_day = 1001", "max_flags_per_day"),
        ("max_compose_per_day = 201", "max_compose_per_day"),
        ("handle_days = 0", "handle_days"),
        ("handle_days = 61", "handle_days"),
        (
            "recipient_domains = [\"not a domain\"]",
            "recipient_domains",
        ),
        ("recipient_domains = [\"localhost\"]", "recipient_domains"),
        (
            "recipient_domains = [\"-bad.example\"]",
            "recipient_domains",
        ),
        ("from_name = \"a\\\"b\"", "from_name"),
        ("from_name = \"<me>\"", "from_name"),
        ("drafts_folder = \"a\\nb\"", "drafts_folder"),
        ("compose_model = \"a\\u0007\"", "compose_model"),
        ("propose = [\"trash\"]", "mail.actions.trash"),
        (
            "trash = \"approve\"\npropose = [\"trash\", \"trash\"]",
            "twice",
        ),
        ("propose = [\"delete\"]", "unknown variant"),
        ("reply_to = \"sometimes\"", "unknown variant"),
        ("sent_copy = \"never\"", "unknown variant"),
    ] {
        let error = parse(&format!("{ROUTE}[actions]\n{extra}\n")).unwrap_err();
        assert!(error.to_string().contains(expected), "{extra}: {error}");
    }
    let long = format!(
        "{ROUTE}[actions]\nreply_instructions = \"{}\"\n",
        "a".repeat(4097)
    );
    assert!(parse(&long).is_err());
    let domains = (0..65)
        .map(|n| format!("\"d{n}.example\""))
        .collect::<Vec<_>>()
        .join(",");
    assert!(
        parse(&format!(
            "{ROUTE}[actions]\nrecipient_domains = [{domains}]\n"
        ))
        .is_err()
    );

    let actions = parse(&format!(
        "{ROUTE}[actions]\nrecipient_domains = [\" @Example.COM \", \"mail.example.org\"]\n\
         drafts_folder = \"  Entwürfe \"\nfrom_name = \" Mei Chen \"\n\
         spam = \"approve\"\narchive = \"approve\"\npropose = [\"spam\", \"archive\"]\n"
    ))
    .unwrap()
    .actions
    .unwrap();
    assert_eq!(
        actions.recipient_domains,
        ["example.com", "mail.example.org"]
    );
    assert_eq!(actions.drafts_folder, "Entwürfe");
    assert_eq!(actions.from_name, "Mei Chen");
    assert_eq!(actions.propose, [Suggestion::Spam, Suggestion::Archive]);
}

#[test]
fn retention_of_actions_is_bounded() {
    for (retention, expected) in [
        ("tombstone_days = 6", "tombstone_days"),
        ("tombstone_days = 366", "tombstone_days"),
        ("unknown_keep_days = 0", "unknown_keep_days"),
        ("max_actions_mib = 0", "max_actions_mib"),
        ("audit_days = 6", "audit_days"),
        ("max_audit_kib = 63", "max_audit_kib"),
    ] {
        let error = parse(&format!("{ROUTE}[retention]\n{retention}\n")).unwrap_err();
        assert!(error.to_string().contains(expected), "{retention}: {error}");
    }
    let settings = parse(ROUTE).unwrap();
    assert_eq!(settings.retention.tombstone_days, 30);
    assert_eq!(settings.retention.unknown_keep_days, 3);
    assert_eq!(settings.retention.audit_days, 90);
}

#[test]
fn every_preview_waiting_at_once_must_fit_in_the_state_file() {
    // 256 queued items × 1.5 KiB and 128 previews × 12 KiB pass three
    // quarters of 2 MiB.
    let error = parse(&format!("{ROUTE}[actions]\nmax_open = 128\n")).unwrap_err();
    assert!(
        error.to_string().contains("mail.actions.max_open"),
        "{error}"
    );
    assert!(
        parse(&format!(
            "{ROUTE}[actions]\nmax_open = 128\n[retention]\nmax_state_kib = 4096\n"
        ))
        .is_ok()
    );
    assert!(parse(&format!("{ROUTE}[actions]\n")).is_ok());
}

#[test]
fn unknown_keys_and_out_of_bounds_values_fail() {
    for (extra, expected) in [
        ("typo = 1\n", "unknown field"),
        ("max_body_kib = 0\n", "mail.max_body_kib"),
        ("max_body_kib = 65\n", "mail.max_body_kib"),
        ("max_fetch_kib = 15\n", "mail.max_fetch_kib"),
        ("poll_seconds = 5\n", "mail.poll_seconds"),
        ("catchup_hours = 0\n", "mail.catchup_hours"),
        ("max_tokens_per_day = 20000000\n", "mail.max_tokens_per_day"),
        ("mailbox = \"\"\n", "mail.mailbox"),
        ("mailbox = \"a\\nb\"\n", "mail.mailbox"),
        ("triage_model = \"a\\u0007\"\n", "mail.triage_model"),
    ] {
        let error = parse(&format!("{extra}{ROUTE}")).unwrap_err();
        assert!(error.to_string().contains(expected), "{extra}: {error}");
    }
    let long = format!("instructions = \"{}\"\n{ROUTE}", "a".repeat(4097));
    assert!(parse(&long).is_err());
    assert!(parse("max_tokens_per_day = 0\n[notify]\nroute = [\"feishu:mail\"]\n").is_ok());
}

#[test]
fn routes_name_one_to_four_chat_accounts() {
    for route in [
        "[]",
        "[\"feishu\"]",
        "[\"email:default\"]",
        "[\"feishu:a b\"]",
        "[\"a:1\",\"a:2\",\"a:3\",\"a:4\",\"a:5\"]",
    ] {
        assert!(
            parse(&format!("[notify]\nroute = {route}\n")).is_err(),
            "{route}"
        );
    }
    let settings = parse("[notify]\nroute = [\"feishu:mail\", \"wechat:mail\"]\n").unwrap();
    assert_eq!(settings.notify.route, ["feishu:mail", "wechat:mail"]);
}

#[test]
fn notification_bounds_are_checked() {
    for (notify, expected) in [
        ("settle_seconds = 3601", "settle_seconds"),
        (
            "settle_seconds = 600\nmax_delay_seconds = 300",
            "max_delay_seconds",
        ),
        ("max_items = 0", "max_items"),
        ("max_message_kib = 16", "max_message_kib"),
        ("max_messages_per_day = 0", "max_messages_per_day"),
        ("quiet_hours = \"23:00\"", "quiet_hours"),
        ("quiet_hours = \"07:00-07:00\"", "quiet_hours"),
        ("quiet_hours = \"24:00-07:00\"", "quiet_hours"),
        ("utc_offset = \"8\"", "utc_offset"),
        ("utc_offset = \"+14:30\"", "utc_offset"),
        ("quiet_urgent = \"sometimes\"", "unknown variant"),
        ("max_queue = 8", "max_queue"),
        ("give_up_hours = 0", "give_up_hours"),
    ] {
        let error = parse(&format!("[notify]\nroute = [\"feishu:mail\"]\n{notify}\n")).unwrap_err();
        assert!(error.to_string().contains(expected), "{notify}: {error}");
    }
    let settings = parse(
        "[notify]\nroute = [\"feishu:mail\"]\nquiet_hours = \"23:00-07:30\"\n\
         utc_offset = \"-03:30\"\nquiet_urgent = \"hold\"\nskipped = \"off\"\n",
    )
    .unwrap();
    assert_eq!(
        settings.notify.quiet().unwrap(),
        Some(QuietHours {
            start: 23 * 60,
            end: 7 * 60 + 30
        })
    );
    assert_eq!(
        settings.notify.fixed_offset().unwrap(),
        Some(-(3 * 3600 + 1800))
    );
    assert_eq!(settings.notify.quiet_urgent, QuietUrgent::Hold);
    assert_eq!(settings.notify.skipped, Skipped::Off);
}

#[test]
fn the_queue_must_fit_in_half_the_state_file() {
    let error = parse(&format!(
        "{ROUTE}max_queue = 512\n[retention]\nmax_state_kib = 1024\n"
    ))
    .unwrap_err();
    assert!(error.to_string().contains("max_queue"), "{error}");
}

#[test]
fn rules_are_lowercased_and_checked() {
    let settings = parse(&format!(
        "{ROUTE}[[rules]]\nfrom = [\" Boss@Example.COM \", \"@Bank.example\"]\naction = \"triage\"\n\
         urgent = true\n[[rules]]\nlist_id = [\"News\"]\naction = \"count\"\n[[rules]]\n\
         bulk = false\nnoreply = true\naction = \"header\"\n"
    ))
    .unwrap();
    assert_eq!(settings.rules.len(), 3);
    assert_eq!(
        settings.rules[0].from,
        ["boss@example.com", "@bank.example"]
    );
    assert!(settings.rules[0].urgent);
    assert_eq!(settings.rules[1].list_id, ["news"]);
    assert_eq!(settings.rules[2].noreply, Some(true));
    for rule in [
        "from = [\"nobody\"]\naction = \"count\"",
        "from = [\"a@\"]\naction = \"count\"",
        "from = [\"\"]\naction = \"count\"",
        "action = \"delete\"",
        "action = \"propose_spam\"",
        "from = [\"a@b\"]",
        "subject = [\"x\"]\naction = \"count\"",
    ] {
        assert!(
            parse(&format!("{ROUTE}[[rules]]\n{rule}\n")).is_err(),
            "{rule}"
        );
    }
}
