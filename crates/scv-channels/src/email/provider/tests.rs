//! Unit tests for `src/email/provider.rs`.

use super::super::credentials::{Grant, Smtp, SmtpSecurity};
use super::*;

/// `mail` settings with `actions` absent, or with `body` as the table.
fn settings(actions: Option<&str>) -> MailSettings {
    let text = match actions {
        None => "[notify]\nroute = [\"fake:mail\"]\n".to_owned(),
        Some(body) => format!("[notify]\nroute = [\"fake:mail\"]\n\n[actions]\n{body}"),
    };
    let table: toml::Table = text.parse().unwrap();
    MailSettings::parse(Some(&table)).unwrap()
}

fn imap_account(smtp: bool) -> Account {
    Account::Imap {
        host: "imap.example.com".into(),
        port: 993,
        username: "me@example.com".into(),
        password: "authorization-code".into(),
        address: None,
        smtp: smtp.then(|| Smtp {
            host: "smtp.example.com".into(),
            port: 465,
            security: SmtpSecurity::Tls,
        }),
    }
}

fn gmail_account() -> Account {
    Account::Gmail {
        address: "me@gmail.com".into(),
        client_id: "client".into(),
        client_secret: None,
    }
}

fn graph_account() -> Account {
    Account::Graph {
        address: "me@outlook.com".into(),
        client_id: "client".into(),
        tenant: "consumers".into(),
    }
}

/// Grants at `directory`, and the parts that load them. A flag left off
/// omits that grant.
fn with_grants(
    directory: &std::path::Path,
    provider: OAuthProvider,
    reader: bool,
    writer: bool,
    sender: bool,
) -> OAuthParts {
    let path = directory.join("account.grants");
    let mut grants = Grants::default();
    for (present, kind) in [
        (reader, GrantKind::Reader),
        (writer, GrantKind::Writer),
        (sender, GrantKind::Sender),
    ] {
        if present {
            grants.set(
                kind,
                Grant {
                    refresh_token: format!("rt-{}", kind.name()).into(),
                    scopes: vec![kind.name().into()],
                },
            );
        }
    }
    grants.save(&path).unwrap();
    let lock_path = directory.join("lock");
    OAuthParts {
        provider,
        endpoints: oauth::Endpoints {
            authorize: "http://127.0.0.1:9/authorize".into(),
            token: "http://127.0.0.1:9/token".into(),
        },
        client_id: "client".into(),
        client_secret: None,
        grants: path,
        lock: Arc::new(move || -> Result<_> {
            Ok(std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)?)
        }),
    }
}

fn adapt(account: &Account, actions: Option<&str>, oauth: Option<OAuthParts>) -> Result<Provider> {
    provider(account, &settings(actions), oauth, &api::Origins::default())
}

/// `provider` does not implement `Debug`, so a refusal is taken apart by hand.
fn failure<T>(result: Result<T>) -> anyhow::Error {
    match result {
        Ok(_) => panic!("expected the provider to refuse"),
        Err(error) => error,
    }
}

#[test]
fn imap_without_actions_has_no_effects_and_send_needs_an_smtp_host() {
    let plain = adapt(&imap_account(false), None, None).unwrap();
    assert_eq!(plain.kind, ProviderKind::Imap);
    assert!(plain.effects.is_none());
    assert!(!plain.can_send);

    let with_smtp = adapt(&imap_account(true), None, None).unwrap();
    assert!(
        with_smtp.effects.is_none(),
        "no actions table, so no writer"
    );
    assert!(with_smtp.can_send, "SMTP is present even while send is off");

    let actions_off = adapt(&imap_account(true), Some(""), None).unwrap();
    assert!(
        actions_off.effects.is_none(),
        "an actions table that turns nothing on loads no writer"
    );

    let archive = adapt(&imap_account(false), Some("archive = \"approve\"\n"), None).unwrap();
    assert!(archive.effects.is_some());
    assert!(!archive.can_send);

    let sending = adapt(&imap_account(true), Some("send = \"approve\"\n"), None).unwrap();
    assert!(sending.effects.is_some());
    assert!(sending.can_send);

    let error = failure(adapt(
        &imap_account(false),
        Some("send = \"approve\"\n"),
        None,
    ));
    assert!(error.to_string().contains("--smtp-host"), "{error:#}");
}

#[test]
fn oauth_without_actions_builds_no_effects_even_when_writing_grants_exist() {
    for (account, kind, oauth_provider) in [
        (gmail_account(), ProviderKind::Gmail, OAuthProvider::Gmail),
        (graph_account(), ProviderKind::Graph, OAuthProvider::Graph),
    ] {
        let home = tempfile::tempdir().unwrap();
        let reader_only = adapt(
            &account,
            None,
            Some(with_grants(home.path(), oauth_provider, true, false, false)),
        )
        .unwrap();
        assert_eq!(reader_only.kind, kind);
        assert!(reader_only.effects.is_none());
        assert!(!reader_only.can_send);

        let home = tempfile::tempdir().unwrap();
        let granted = adapt(
            &account,
            None,
            Some(with_grants(home.path(), oauth_provider, true, true, true)),
        )
        .unwrap();
        assert!(
            granted.effects.is_none(),
            "writing grants exist, but nothing may act, so no writer is loaded"
        );
        assert!(granted.can_send);

        let home = tempfile::tempdir().unwrap();
        let table_off = adapt(
            &account,
            Some(""),
            Some(with_grants(home.path(), oauth_provider, true, true, true)),
        )
        .unwrap();
        assert!(table_off.effects.is_none());

        let missing = failure(adapt(&account, None, None));
        assert!(
            missing.to_string().contains("grants are not available"),
            "{missing:#}"
        );
    }
}

#[test]
fn a_missing_reader_or_the_grant_an_action_needs_is_named_in_the_error() {
    for (account, oauth_provider) in [
        (gmail_account(), OAuthProvider::Gmail),
        (graph_account(), OAuthProvider::Graph),
    ] {
        let home = tempfile::tempdir().unwrap();
        let error = failure(adapt(
            &account,
            None,
            Some(with_grants(home.path(), oauth_provider, false, true, true)),
        ));
        assert!(error.to_string().contains("reading grant"), "{error:#}");

        let home = tempfile::tempdir().unwrap();
        let error = failure(adapt(
            &account,
            Some("archive = \"approve\"\n"),
            Some(with_grants(home.path(), oauth_provider, true, false, true)),
        ));
        assert!(error.to_string().contains("--write"), "{error:#}");
    }

    let home = tempfile::tempdir().unwrap();
    let gmail_send = failure(adapt(
        &gmail_account(),
        Some("send = \"approve\"\n"),
        Some(with_grants(
            home.path(),
            OAuthProvider::Gmail,
            true,
            false,
            true,
        )),
    ));
    assert!(
        gmail_send.to_string().contains("--send"),
        "Gmail sends with the writer grant: {gmail_send:#}"
    );

    let home = tempfile::tempdir().unwrap();
    let graph_send = failure(adapt(
        &graph_account(),
        Some("send = \"approve\"\n"),
        Some(with_grants(
            home.path(),
            OAuthProvider::Graph,
            true,
            true,
            false,
        )),
    ));
    assert!(graph_send.to_string().contains("--send"), "{graph_send:#}");
}

#[test]
fn only_the_grant_an_action_uses_is_required() {
    let home = tempfile::tempdir().unwrap();
    let graph_send = adapt(
        &graph_account(),
        Some("send = \"approve\"\n"),
        Some(with_grants(
            home.path(),
            OAuthProvider::Graph,
            true,
            false,
            true,
        )),
    )
    .unwrap();
    assert_eq!(graph_send.kind, ProviderKind::Graph);
    assert!(graph_send.effects.is_some());
    assert!(
        graph_send.can_send,
        "the sender grant is what lets Graph send"
    );

    let home = tempfile::tempdir().unwrap();
    let graph_archive = adapt(
        &graph_account(),
        Some("archive = \"approve\"\n"),
        Some(with_grants(
            home.path(),
            OAuthProvider::Graph,
            true,
            true,
            false,
        )),
    )
    .unwrap();
    assert!(graph_archive.effects.is_some());
    assert!(
        !graph_archive.can_send,
        "a writer grant does not let Graph send"
    );

    let home = tempfile::tempdir().unwrap();
    let gmail_send = adapt(
        &gmail_account(),
        Some("send = \"approve\"\n"),
        Some(with_grants(
            home.path(),
            OAuthProvider::Gmail,
            true,
            true,
            false,
        )),
    )
    .unwrap();
    assert_eq!(gmail_send.kind, ProviderKind::Gmail);
    assert!(gmail_send.effects.is_some());
    assert!(gmail_send.can_send);

    let home = tempfile::tempdir().unwrap();
    let gmail_archive = adapt(
        &gmail_account(),
        Some("mark_read = \"approve\"\n"),
        Some(with_grants(
            home.path(),
            OAuthProvider::Gmail,
            true,
            true,
            false,
        )),
    )
    .unwrap();
    assert!(gmail_archive.effects.is_some());
    assert!(
        gmail_archive.can_send,
        "Gmail's writer grant is also the grant that can send"
    );
}
