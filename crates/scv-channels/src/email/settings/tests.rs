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
fn mail_actions_are_refused_in_this_release() {
    for table in [
        "[actions]\nsend = \"approve\"\n",
        "[actions]\ndraft = \"off\"\n",
    ] {
        let error = parse(&format!("{ROUTE}{table}")).unwrap_err();
        assert!(
            error.to_string().contains("not available in this release"),
            "{error}"
        );
    }
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
