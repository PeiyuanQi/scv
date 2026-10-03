use crate::cli::args::Cli;
use clap::Parser as _;

#[test]
fn slack_login_requires_manual_secrets_and_accepts_owner_without_secret_flags() {
    assert!(
        Cli::try_parse_from([
            "scv",
            "channels",
            "login",
            "slack",
            "--slack-owner-user-id",
            "U123"
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "scv",
            "channels",
            "login",
            "slack",
            "--slack-bot-token",
            "xoxb-secret"
        ])
        .is_err()
    );
    assert!(Cli::try_parse_from(["scv", "channels", "login", "feishu"]).is_ok());
    assert!(Cli::try_parse_from(["scv", "channels", "login", "lark"]).is_ok());
}
