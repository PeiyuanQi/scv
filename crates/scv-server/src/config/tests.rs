//! Unit tests for `src/config/`.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::PathBuf,
    time::Duration,
};

use scv_channels::state::AccountSettings;
use scv_tools::web::SearchBackend;

use super::{
    load::{is_secret_key, merge},
    validate::{
        MAX_BACKGROUND_JOBS, MAX_PROVIDER_RETRIES, MAX_TOOL_TIMEOUT_SECONDS, MAX_USE_FOR_BYTES,
        valid_domain_pattern, validate_project_keys, validate_project_not_weaker,
    },
    *,
};

#[test]
fn project_cannot_redirect_provider_or_agent() {
    let provider: toml::Value = toml::from_str(
        r#"[provider]
base_url = "https://attacker.invalid"
"#,
    )
    .unwrap();
    assert!(validate_project_keys(&provider).is_err());

    let agent: toml::Value = toml::from_str(
        r#"[agents.codex]
command = "/tmp/fake"
"#,
    )
    .unwrap();
    assert!(validate_project_keys(&agent).is_err());
}

#[test]
fn channel_accounts_are_user_only_and_validated() {
    let project: toml::Value =
        toml::from_str("[channels.wechat.default]\nenabled = false\n").unwrap();
    assert!(validate_project_keys(&project).is_err());

    let settings = |workspace: Option<&str>| AccountSettings {
        workspace: workspace.map(PathBuf::from),
        ..AccountSettings::default()
    };
    let with = |channel: &str, account: &str, workspace: Option<&str>| Config {
        channels: BTreeMap::from([(
            channel.to_owned(),
            BTreeMap::from([(account.to_owned(), settings(workspace))]),
        )]),
        ..Config::default()
    };
    assert!(
        with("wechat", "default", Some("/srv/work"))
            .validate()
            .is_ok()
    );
    assert!(with("feishu", "team-2", None).validate().is_ok());
    let unknown = with("irc", "default", None).validate().unwrap_err();
    assert!(unknown.to_string().contains("wechat, feishu"), "{unknown}");
    assert!(with("wechat", "a.b", None).validate().is_err());
    assert!(with("wechat", "default", Some("work")).validate().is_err());
    let parsed: Config =
        toml::from_str("[channels.wechat.default]\nenabled = true\nremote_tools = \"owner\"\n")
            .unwrap();
    assert_eq!(
        parsed.channels["wechat"]["default"].remote_tools,
        scv_channels::state::RemoteTools::Owner
    );
    assert!(toml::from_str::<Config>("[channels.wechat.default]\nenabeld = true\n").is_err());
}

#[test]
fn keys_holding_credentials_are_hidden_but_limits_are_not() {
    for secret in [
        "providers.openai.api_key",
        "provider.api_key",
        "web.brave_api_key",
        "providers.x.headers.Authorization",
        "anything.client_secret",
    ] {
        assert!(is_secret_key(secret), "{secret}");
    }
    for plain in [
        "provider.api_key_env",
        "web.brave_api_key_env",
        "context.max_tokens",
        "context.reserve_output_tokens",
        "providers.openai.base_url",
    ] {
        assert!(!is_secret_key(plain), "{plain}");
    }
}

#[test]
fn project_may_tighten_but_not_weaken_limits() {
    let user = Config::default();
    let mut tighter = user.clone();
    tighter.tools.output_limit_bytes /= 2;
    tighter.tools.approval_policy = ApprovalPolicy::Always;
    assert!(validate_project_not_weaker(&user, &tighter).is_ok());

    let mut weaker = user.clone();
    weaker.tools.output_limit_bytes *= 2;
    assert!(validate_project_not_weaker(&user, &weaker).is_err());
}

#[test]
fn timeouts_default_below_a_ceiling_that_projects_may_only_lower() {
    let user = Config::default();
    assert_eq!(
        (
            user.tools.command_timeout_seconds,
            user.tools.agent_timeout_seconds,
            user.tools.max_timeout_seconds
        ),
        (600, 3600, 14400)
    );
    assert_eq!(user.agent.max_steps, 128);
    assert_eq!(user.provider.timeout_seconds, 600);
    let tools = user.tools();
    assert_eq!(tools.command_timeout, Duration::from_secs(600));
    assert_eq!(tools.agent_timeout, Duration::from_secs(3600));
    assert_eq!(tools.max_timeout, Duration::from_secs(14400));
    // A ClawBot owner turn outlasts the ceiling by five minutes: 4h05m.
    assert_eq!(
        scv_clawbot::owner_turn_timeout(tools.max_timeout),
        Duration::from_secs(4 * 3600 + 5 * 60)
    );

    for (field, name) in [
        (0, "tools.command_timeout_seconds"),
        (1, "tools.agent_timeout_seconds"),
    ] {
        let mut config = Config::default();
        let value = if field == 0 {
            &mut config.tools.command_timeout_seconds
        } else {
            &mut config.tools.agent_timeout_seconds
        };
        *value = config.tools.max_timeout_seconds + 1;
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            format!("{name} exceeds tools.max_timeout_seconds")
        );
    }
    let mut unbounded = Config::default();
    unbounded.tools.max_timeout_seconds = MAX_TOOL_TIMEOUT_SECONDS + 1;
    assert!(unbounded.validate().is_err());
    let mut zero = Config::default();
    zero.tools.agent_timeout_seconds = 0;
    assert!(zero.validate().is_err());

    let mut lower = user.clone();
    lower.tools.max_timeout_seconds = 900;
    lower.tools.agent_timeout_seconds = 300;
    assert!(validate_project_not_weaker(&user, &lower).is_ok());
    for raise in [
        |config: &mut Config| config.tools.max_timeout_seconds += 1,
        |config: &mut Config| config.tools.agent_timeout_seconds += 1,
    ] {
        let mut higher = user.clone();
        raise(&mut higher);
        assert!(validate_project_not_weaker(&user, &higher).is_err());
    }
}

#[test]
fn conversation_limits_are_positive_and_projects_may_only_lower_them() {
    let user = Config::default();
    assert_eq!(
        (
            user.agent.max_conversations,
            user.agent.conversation_idle_seconds
        ),
        (8, 86400)
    );
    let limits = user.tools().conversations;
    assert_eq!((limits.max, limits.idle), (8, Duration::from_secs(86400)));
    for zero in [
        |config: &mut Config| config.agent.max_conversations = 0,
        |config: &mut Config| config.agent.conversation_idle_seconds = 0,
    ] {
        let mut config = Config::default();
        zero(&mut config);
        assert!(config.validate().is_err());
    }
    let mut lower = user.clone();
    lower.agent.max_conversations = 2;
    lower.agent.conversation_idle_seconds = 600;
    assert!(validate_project_not_weaker(&user, &lower).is_ok());
    for raise in [
        |config: &mut Config| config.agent.max_conversations += 1,
        |config: &mut Config| config.agent.conversation_idle_seconds += 1,
    ] {
        let mut higher = user.clone();
        raise(&mut higher);
        assert!(validate_project_not_weaker(&user, &higher).is_err());
    }
}

#[test]
fn agent_choice_settings_are_validated_and_user_only() {
    let mut config = Config::default();
    config.agent.prefer = vec!["codex".into(), "claude".into()];
    config.agents.0.get_mut("grok").unwrap().use_for = Some("current events and X posts".into());
    assert!(config.validate().is_ok());
    let adapters = config.adapters();
    assert_eq!(
        adapters["agent_grok"].use_for.as_deref(),
        Some("current events and X posts")
    );
    assert_eq!(adapters["agent_codex"].use_for, None);
    let mut unknown = Config::default();
    unknown.agent.prefer = vec!["zcode".into()];
    assert!(unknown.validate().is_err());
    for bad in ["", "two\nlines", &"x".repeat(MAX_USE_FOR_BYTES + 1)] {
        let mut config = Config::default();
        config.agents.0.get_mut("codex").unwrap().use_for = Some(bad.to_owned());
        assert!(config.validate().is_err(), "{bad:?}");
    }
    let project: toml::Value = toml::from_str("[agent]\nprefer = [\"pi\"]\n").unwrap();
    assert!(validate_project_keys(&project).is_err());
}

#[test]
fn background_jobs_are_bounded_and_projects_may_only_lower_them() {
    let user = Config::default();
    assert_eq!(user.agent.max_background, 4);
    assert_eq!(user.tools().max_background, 4);
    let mut off = Config::default();
    off.agent.max_background = 0;
    assert!(off.validate().is_ok(), "0 turns background delegation off");
    let mut many = Config::default();
    many.agent.max_background = MAX_BACKGROUND_JOBS + 1;
    assert!(many.validate().is_err());
    let mut lower = user.clone();
    lower.agent.max_background = 1;
    assert!(validate_project_not_weaker(&user, &lower).is_ok());
    let mut higher = user.clone();
    higher.agent.max_background = 5;
    assert!(validate_project_not_weaker(&user, &higher).is_err());
}

#[test]
fn provider_retries_are_bounded_and_projects_may_only_lower_them() {
    let user = Config::default();
    assert_eq!(user.provider_limits.max_retries, 2);
    assert_eq!(user.provider_limits().max_retries, 2);
    let mut none = user.clone();
    none.provider_limits.max_retries = 0;
    assert!(none.validate().is_ok());
    assert!(validate_project_not_weaker(&user, &none).is_ok());
    assert!(validate_project_not_weaker(&none, &user).is_err());
    let mut excessive = user.clone();
    excessive.provider_limits.max_retries = MAX_PROVIDER_RETRIES + 1;
    assert_eq!(
        excessive.validate().unwrap_err().to_string(),
        format!("provider_limits.max_retries must be at most {MAX_PROVIDER_RETRIES}")
    );
}

#[test]
fn projects_may_disable_but_not_enable_project_skill_scanning() {
    let user = Config::default();
    let mut disabled = user.clone();
    disabled.skills.scan_projects = false;
    assert!(validate_project_not_weaker(&user, &disabled).is_ok());
    assert!(validate_project_not_weaker(&disabled, &user).is_err());
}

#[test]
fn web_defaults_offer_fetch_without_search_and_validate_their_settings() {
    let config = Config::default();
    assert!(config.web.enabled);
    assert_eq!(config.web.search, WebSearchMode::Off);
    assert!(!config.hosted_web_search());
    let tools = config.web_tools().unwrap();
    assert!(tools.search.is_none());
    assert!(!tools.allow_private_addresses);
    assert_eq!(tools.fetch_max_bytes, 2 * 1024 * 1024);
    assert_eq!(tools.output_limit, config.tools.output_limit_bytes);
    assert!(tools.auto_approve_domains.contains(&"docs.rs".to_owned()));

    let mut disabled = Config::default();
    disabled.web.enabled = false;
    disabled.web.search = WebSearchMode::Provider;
    assert!(disabled.web_tools().is_none());
    assert!(!disabled.hosted_web_search());

    let mut provider = Config::default();
    provider.web.search = WebSearchMode::Provider;
    assert!(provider.hosted_web_search());
    assert!(provider.web_tools().unwrap().search.is_none());

    let mut searxng = Config::default();
    searxng.web.search = WebSearchMode::Searxng;
    assert!(
        searxng
            .validate()
            .unwrap_err()
            .to_string()
            .contains("web.searxng_url")
    );
    searxng.web.searxng_url = Some("https://searx.example".into());
    assert!(searxng.validate().is_ok());
    assert!(matches!(
        searxng.web_tools().unwrap().search,
        Some(SearchBackend::Searxng { .. })
    ));

    let mut brave = Config::default();
    brave.web.search = WebSearchMode::Brave;
    brave.web.brave_api_key_env = None;
    assert!(
        brave
            .validate()
            .unwrap_err()
            .to_string()
            .contains("brave_api_key")
    );
    brave.web.brave_api_key = Some("inline-test-key".into());
    assert!(matches!(
        brave.web_tools().unwrap().search,
        Some(SearchBackend::Brave { ref api_key, .. }) if api_key == "inline-test-key"
    ));
    brave.web.brave_api_key = None;
    brave.web.brave_api_key_env = Some("SCV_TEST_UNSET_BRAVE_KEY_VARIABLE".into());
    assert!(brave.validate().is_ok());
    assert!(brave.web_tools().unwrap().search.is_none());

    for (mutate, message) in [
        (
            (|config: &mut Config| {
                config.web.auto_approve_domains = vec!["https://docs.rs/".into()];
            }) as fn(&mut Config),
            "web.auto_approve_domains",
        ),
        (|config| config.web.max_redirects = 11, "web.max_redirects"),
        (
            |config| config.web.fetch_max_bytes = 0,
            "web.fetch_max_bytes",
        ),
        (
            |config| config.web.fetch_timeout_seconds = config.tools.max_timeout_seconds + 1,
            "web.fetch_timeout_seconds",
        ),
        (
            |config| config.web.max_search_results = 21,
            "web.max_search_results",
        ),
    ] {
        let mut config = Config::default();
        mutate(&mut config);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains(message), "{error}");
    }
    for valid in ["docs.rs", "*.example.com", "a-b.c1.dev"] {
        assert!(valid_domain_pattern(valid), "{valid}");
    }
    for invalid in ["", "*.", "docs.rs/path", "-a.com", "a..b", "*", "user@host"] {
        assert!(!valid_domain_pattern(invalid), "{invalid}");
    }
}

#[test]
fn projects_may_narrow_but_not_widen_web_access() {
    for key in [
        "auto_approve_domains = [\"attacker.test\"]",
        "allow_private_addresses = true",
        "searxng_url = \"http://attacker.test\"",
        "brave_url = \"http://attacker.test\"",
        "brave_api_key_env = \"OTHER\"",
    ] {
        let project: toml::Value = toml::from_str(&format!("[web]\n{key}\n")).unwrap();
        assert!(validate_project_keys(&project).is_err(), "{key}");
    }
    let allowed: toml::Value =
        toml::from_str("[web]\nenabled = false\nsearch = \"off\"\nmax_redirects = 1\n").unwrap();
    assert!(validate_project_keys(&allowed).is_ok());

    let mut user = Config::default();
    user.web.search = WebSearchMode::Provider;
    let mut narrower = user.clone();
    narrower.web.enabled = false;
    narrower.web.search = WebSearchMode::Off;
    narrower.web.fetch_max_bytes = 1024;
    narrower.web.max_redirects = 0;
    assert!(validate_project_not_weaker(&user, &narrower).is_ok());
    assert!(validate_project_not_weaker(&narrower, &user).is_err());
    let mut switched = user.clone();
    switched.web.search = WebSearchMode::Searxng;
    assert!(validate_project_not_weaker(&user, &switched).is_err());
    let mut larger = user.clone();
    larger.web.fetch_timeout_seconds += 1;
    assert!(validate_project_not_weaker(&user, &larger).is_err());
}

#[test]
fn cross_field_validation_accounts_for_json_escaping() {
    let mut config = Config::default();
    config.protocol.max_server_frame_bytes = config.provider_limits.max_assistant_bytes;
    assert!(config.validate().is_err());
}

#[test]
fn adapter_selection_templates_survive_partial_overrides_and_validate() {
    let mut value: toml::Value =
        toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
    merge(
        &mut value,
        toml::from_str(
            r#"[agents.claude]
args = ["-p", "--permission-mode", "acceptEdits"]
"#,
        )
        .unwrap(),
    );
    let config: Config = value.try_into().unwrap();
    let claude = &config.agents.0["claude"];
    assert_eq!(claude.args.len(), 3);
    assert_eq!(claude.model_args, ["--model", "{model}"]);
    assert_eq!(claude.effort_args, ["--effort", "{effort}"]);
    assert_eq!(
        config.agents.0["pi"].effort_args,
        ["--thinking", "{effort}"]
    );
    assert_eq!(config.agents.0["grok"].prompt_args, ["-p"]);

    let mut invalid = Config::default();
    invalid.agents.0.get_mut("claude").unwrap().effort_args = vec!["--effort".into()];
    assert!(
        invalid
            .validate()
            .unwrap_err()
            .to_string()
            .contains("agents.claude.effort_args must contain {effort}")
    );
}

#[test]
fn adapters_are_bound_to_the_instance_home() {
    let config = Config {
        instance_home: PathBuf::from("/tmp/scv-instance"),
        ..Config::default()
    };
    let adapters = config.adapters();
    let codex = &adapters["agent_codex"];
    assert!(codex.environment.contains(&(
        OsString::from("CODEX_HOME"),
        OsString::from("/tmp/scv-instance/agents/codex")
    )));
    assert!(codex.environment.contains(&(
        OsString::from("SCV_HOME"),
        OsString::from("/tmp/scv-instance/agents/codex")
    )));
    for (agent, variable, path) in [
        ("grok", "GROK_HOME", "/tmp/scv-instance/agents/grok/.grok"),
        ("dsh", "DSH_HOME", "/tmp/scv-instance/agents/dsh/.dsh"),
        (
            "pi",
            "PI_CODING_AGENT_DIR",
            "/tmp/scv-instance/agents/pi/.pi/agent",
        ),
    ] {
        let adapter = &adapters[&format!("agent_{agent}")];
        assert!(
            adapter
                .environment
                .contains(&(OsString::from(variable), OsString::from(path))),
            "{agent}"
        );
        assert!(adapter.environment.contains(&(
            OsString::from("HOME"),
            OsString::from(format!("/tmp/scv-instance/agents/{agent}"))
        )));
    }
    assert!(adapters["agent_grok"].environment.contains(&(
        OsString::from("GROK_DISABLE_AUTOUPDATER"),
        OsString::from("1")
    )));
    assert_eq!(adapters["agent_grok"].prompt_args, ["-p"]);
    assert!(adapters["agent_pi"].model_hint.contains("provider scv"));
}

#[test]
fn full_codex_over_acp_keeps_live_web_search() {
    let codex_acp = |permissions: &str| {
        let mut value: toml::Value =
            toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
        merge(
            &mut value,
            toml::from_str(&format!(
                "[agents.codex]\npermissions = \"{permissions}\"\n"
            ))
            .unwrap(),
        );
        let config: Config = value.try_into().unwrap();
        config.validate().unwrap();
        config.adapters()["agent_codex"].acp.clone().unwrap()
    };
    let full = codex_acp("full");
    assert_eq!(full.full_mode.as_deref(), Some("agent-full-access"));
    let [(variable, value)] = full.environment.as_slice() else {
        panic!("expected one ACP variable: {:?}", full.environment);
    };
    assert_eq!(variable, "CODEX_CONFIG");
    let overrides: serde_json::Value = serde_json::from_str(value.to_str().unwrap()).unwrap();
    assert_eq!(overrides, serde_json::json!({"web_search": "live"}));
    assert!(
        codex_acp("default").environment.is_empty(),
        "default permissions leave web search to the Codex config"
    );
    assert!(
        scv_tools::adapters::is_removed_agent_variable(std::ffi::OsStr::new("CODEX_CONFIG")),
        "an inherited CODEX_CONFIG never reaches a delegated Codex"
    );
}

#[test]
fn agents_prefer_their_acp_server_unless_configured_otherwise() {
    let defaults = Config::default().adapters();
    let launch = |adapters: &HashMap<String, scv_tools::AgentAdapterConfig>, agent: &str| {
        adapters[&format!("agent_{agent}")].acp.clone()
    };
    for agent in ["claude", "codex", "grok", "dsh"] {
        let acp = launch(&defaults, agent).unwrap();
        assert!(!acp.required, "{agent}: auto falls back to resume");
        assert_eq!(acp.full_mode, None, "{agent}: no full mode by default");
    }
    assert_eq!(
        launch(&defaults, "claude").unwrap().command,
        "claude-agent-acp"
    );
    assert_eq!(launch(&defaults, "codex").unwrap().command, "codex-acp");
    assert_eq!(launch(&defaults, "grok").unwrap().args, ["agent", "stdio"]);
    assert_eq!(launch(&defaults, "dsh").unwrap().args, ["--profile", "acp"]);
    assert!(launch(&defaults, "pi").is_none());
    assert!(launch(&defaults, "scv").is_none());

    let mut value: toml::Value =
        toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
    merge(
        &mut value,
        toml::from_str(
            "[agents.claude]\npermissions = \"full\"\ntransport = \"acp\"\n\n\
                 [agents.codex]\ntransport = \"resume\"\n\n\
                 [agents.grok]\npermissions = \"full\"\n",
        )
        .unwrap(),
    );
    let config: Config = value.try_into().unwrap();
    config.validate().unwrap();
    let adapters = config.adapters();
    let claude = launch(&adapters, "claude").unwrap();
    assert!(claude.required);
    assert_eq!(claude.full_mode.as_deref(), Some("bypassPermissions"));
    assert!(launch(&adapters, "codex").is_none(), "resume turns ACP off");

    let mut custom: toml::Value =
        toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
    merge(
        &mut custom,
        toml::from_str(
            "[agents.claude]\ncommand = \"/opt/claude-wrapper\"\n\n\
                 [agents.codex]\nargs = [\"exec\", \"--skip-git-repo-check\"]\n",
        )
        .unwrap(),
    );
    let custom: Config = custom.try_into().unwrap();
    let custom = custom.adapters();
    assert!(
        launch(&custom, "claude").is_none(),
        "a custom command keeps one process per turn"
    );
    assert!(launch(&custom, "codex").is_some(), "custom args keep ACP");
    assert_eq!(
        launch(&adapters, "grok").unwrap().args,
        ["agent", "--always-approve", "stdio"]
    );

    let mut pi: toml::Value =
        toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
    merge(
        &mut pi,
        toml::from_str("[agents.pi]\ntransport = \"acp\"\n").unwrap(),
    );
    let pi: Config = pi.try_into().unwrap();
    let error = pi.validate().unwrap_err().to_string();
    assert!(error.contains("no verified ACP server"), "{error}");

    let mut invalid: toml::Value =
        toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
    merge(
        &mut invalid,
        toml::from_str("[agents.claude]\ntransport = \"rpc\"\n").unwrap(),
    );
    assert!(invalid.try_into::<Config>().is_err());
}

#[test]
fn full_permissions_are_opt_in_per_agent_and_combine_with_args() {
    let defaults = Config::default().adapters();
    for adapter in defaults.values() {
        assert_eq!(adapter.full_permission_args, None);
    }
    let mut value: toml::Value =
        toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
    merge(
            &mut value,
            toml::from_str(
                "[agents.claude]\npermissions = \"full\"\n\n\
                 [agents.codex]\nargs = [\"exec\", \"--skip-git-repo-check\"]\npermissions = \"full\"\n\n\
                 [agents.grok]\npermissions = \"full\"\n\n\
                 [agents.dsh]\npermissions = \"full\"\n\n\
                 [agents.pi]\npermissions = \"full\"\n",
            )
            .unwrap(),
        );
    let config: Config = value.try_into().unwrap();
    config.validate().unwrap();
    let adapters = config.adapters();
    let full = |agent: &str| {
        adapters[&format!("agent_{agent}")]
            .full_permission_args
            .clone()
            .unwrap()
    };
    assert_eq!(full("claude"), ["--permission-mode", "bypassPermissions"]);
    assert_eq!(
        full("codex"),
        [
            "--dangerously-bypass-approvals-and-sandbox",
            "-c",
            "web_search=\"live\""
        ]
    );
    assert_eq!(
        adapters["agent_codex"].args,
        ["exec", "--skip-git-repo-check"]
    );
    assert_eq!(full("grok"), ["--always-approve"]);
    assert!(full("dsh").is_empty());
    assert!(adapters["agent_dsh"].environment.contains(&(
        OsString::from("DSH_PERMISSION_MODE"),
        OsString::from("danger-full-access")
    )));
    assert!(
        !defaults["agent_dsh"]
            .environment
            .iter()
            .any(|(variable, _)| variable == "DSH_PERMISSION_MODE")
    );
    // pi has no permission system: `full` is accepted and adds nothing.
    assert!(full("pi").is_empty());

    let mut invalid: toml::Value =
        toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
    merge(
        &mut invalid,
        toml::from_str("[agents.claude]\npermissions = \"yolo\"\n").unwrap(),
    );
    assert!(invalid.try_into::<Config>().is_err());
}

#[test]
fn user_agent_overrides_merge_over_every_built_in_and_unknown_agents_fail() {
    let mut value: toml::Value =
        toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
    merge(
        &mut value,
        toml::from_str(
            "[agents.pi]
model_args = []

[agents.grok]
args = [\"--always-approve\"]
",
        )
        .unwrap(),
    );
    let config: Config = value.clone().try_into().unwrap();
    assert!(config.agents.0["pi"].model_args.is_empty());
    assert_eq!(config.agents.0["pi"].args, ["-p"]);
    assert_eq!(config.agents.0["grok"].args, ["--always-approve"]);
    assert_eq!(config.agents.0["grok"].prompt_args, ["-p"]);
    assert_eq!(
        config.agents.0.keys().collect::<Vec<_>>(),
        ["claude", "codex", "dsh", "grok", "pi", "scv"]
    );

    merge(
        &mut value,
        toml::from_str(
            "[agents.zcode]
command = \"zcode\"
",
        )
        .unwrap(),
    );
    let unknown: Config = value.try_into().unwrap();
    let error = unknown.validate().unwrap_err().to_string();
    assert!(error.contains("unknown agent [agents.zcode]"), "{error}");
}
