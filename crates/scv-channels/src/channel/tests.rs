//! Unit tests for `src/channel.rs`.

use super::*;

#[test]
fn channels_keep_their_names_and_order() {
    let names: Vec<_> = ChannelKind::ALL.iter().map(|kind| kind.name()).collect();
    assert_eq!(names, ["wechat", "feishu"]);
    assert_eq!(ChannelKind::parse("wechat").unwrap(), ChannelKind::Wechat);
    assert_eq!(ChannelKind::parse("feishu").unwrap(), ChannelKind::Feishu);
    // Lark is a Feishu brand, not a channel.
    assert_eq!(
        ChannelKind::parse("lark").unwrap_err().to_string(),
        "Unknown channel \"lark\""
    );
    assert_eq!(ChannelKind::Wechat.title(), "WeChat");
    assert_eq!(ChannelKind::Feishu.title(), "Feishu");
}

fn wechat() -> crate::wechat::Account {
    crate::wechat::Account {
        token: "token".into(),
        base_url: "https://ilinkai.weixin.qq.com".into(),
        bot_id: Some("bot@im.bot".into()),
        user_id: Some("owner@im.wechat".into()),
    }
}

fn lark() -> crate::feishu::Account {
    crate::feishu::Account {
        app_id: "cli_a1b2c3d4".into(),
        app_secret: "secret".into(),
        brand: crate::feishu::Brand::Lark,
        owner_open_id: None,
    }
}

#[test]
fn credentials_name_their_owner_bot_and_platform() {
    assert_eq!(crate::wechat::WeChat::title(&wechat()), "WeChat");
    let wechat = ChannelCredentials::from(wechat());
    assert!(matches!(wechat, ChannelCredentials::Wechat(_)));
    assert_eq!(wechat.owner(), Some("owner@im.wechat"));
    assert_eq!(wechat.bot_id().as_deref(), Some("bot@im.bot"));
    assert_eq!(crate::feishu::Feishu::title(&lark()), "Lark");
    let lark = ChannelCredentials::from(lark());
    assert!(matches!(lark, ChannelCredentials::Feishu(_)));
    assert_eq!(lark.owner(), None);
    assert_eq!(lark.bot_id().as_deref(), Some("cli_a1b2c3d4"));
}

#[test]
fn accounts_read_and_write_each_channel_under_the_layout() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    crate::wechat::Store::new(&layout, "wechat")
        .save_account("default", &wechat())
        .unwrap();
    let wechat_accounts = ChannelKind::Wechat.accounts(&layout);
    let feishu_accounts = ChannelKind::Feishu.accounts(&layout);
    assert_eq!(wechat_accounts.kind(), ChannelKind::Wechat);
    assert_eq!(wechat_accounts.names().unwrap(), ["default"]);
    assert!(feishu_accounts.names().unwrap().is_empty());
    assert!(wechat_accounts.signed_in("default").unwrap());
    assert!(!feishu_accounts.signed_in("default").unwrap());
    assert_eq!(
        wechat_accounts.credentials_path("default").unwrap(),
        home.path().join("credentials/wechat/default.json")
    );

    let settings = AccountSettings {
        enabled: false,
        ..AccountSettings::default()
    };
    wechat_accounts.save_settings("default", &settings).unwrap();
    assert_eq!(wechat_accounts.settings("default").unwrap(), settings);
    assert_eq!(wechat_accounts.configured().unwrap(), ["default"]);
    let (credentials, snapshot) = wechat_accounts.snapshot("default").unwrap();
    assert!(credentials == Some(ChannelCredentials::from(wechat())));
    assert_eq!(snapshot, settings);
    let (credentials, _) = wechat_accounts.inspect("default");
    assert!(credentials.unwrap().is_some());

    wechat_accounts.remove("default").unwrap();
    assert!(wechat_accounts.names().unwrap().is_empty());
    assert!(wechat_accounts.configured().unwrap().is_empty());
}

#[test]
fn the_bridge_grants_tools_only_with_an_owner_and_a_turn_timeout() {
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::new(home.path());
    let credentials = ChannelCredentials::from(wechat());
    let settings = AccountSettings::default();
    let link = hub::Link::detached();
    let run = AccountRun {
        layout: &layout,
        account: "default",
        credentials: &credentials,
        settings: &settings,
        owner: Some("owner@im.wechat"),
        tool_turn_timeout: None,
        workspace: home.path(),
        socket: &home.path().join("state/server.sock"),
        link: &link,
        health: &|_| {},
    };
    let bridge = run.bridge(ChannelKind::Wechat).unwrap();
    assert_eq!(bridge.owner, Some("owner@im.wechat"));
    assert_eq!(bridge.tool_owner, None);
    // Only the owner is answered unless the settings say anyone.
    assert_eq!(bridge.senders, state::Senders::Owner);
    let anyone = AccountSettings {
        senders: state::Senders::Anyone,
        ..AccountSettings::default()
    };
    let open = AccountRun {
        settings: &anyone,
        ..run
    };
    assert_eq!(
        open.bridge(ChannelKind::Wechat).unwrap().senders,
        state::Senders::Anyone
    );
    assert_eq!(
        bridge.media.inbox,
        home.path().join("state/media/wechat/default")
    );
    let granted = AccountRun {
        tool_turn_timeout: Some(Duration::from_secs(1800)),
        ..run
    };
    assert_eq!(
        granted.bridge(ChannelKind::Wechat).unwrap().tool_owner,
        Some(ToolOwner {
            user_id: "owner@im.wechat".into(),
            turn_timeout: Duration::from_secs(1800),
        })
    );
    let nobody = AccountRun {
        owner: None,
        ..granted
    };
    assert_eq!(nobody.bridge(ChannelKind::Wechat).unwrap().tool_owner, None);
    let invalid = AccountRun {
        account: "../default",
        ..run
    };
    assert!(invalid.bridge(ChannelKind::Wechat).is_err());
}
