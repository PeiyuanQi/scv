//! Unit tests for `src/delegate/adapters.rs`.

use super::*;

#[test]
fn descriptors_are_unique_and_self_consistent() {
    let mut names: Vec<_> = ADAPTERS.iter().map(|adapter| adapter.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), ADAPTERS.len());
    for adapter in ADAPTERS {
        assert!(
            adapter.model_args.is_empty()
                || adapter.model_args.iter().any(|arg| arg.contains("{model}")),
            "{}",
            adapter.name
        );
        assert!(
            adapter.effort_args.is_empty()
                || adapter
                    .effort_args
                    .iter()
                    .any(|arg| arg.contains("{effort}")),
            "{}",
            adapter.name
        );
        if let Resume::Supported {
            start,
            subcommand,
            options,
            positional,
        } = adapter.resume
        {
            let names_session = |args: &[&str]| args.iter().any(|arg| arg.contains("{session}"));
            assert!(start.is_empty() || names_session(start), "{}", adapter.name);
            assert!(
                names_session(options) || names_session(positional),
                "{}",
                adapter.name
            );
            assert!(!names_session(subcommand), "{}", adapter.name);
            // Cursor CLI's chat store layout is not verified, so `scv agents
            // gc` leaves its transcripts alone.
            assert!(
                adapter.conversation_files.is_some() || adapter.name == "cursor",
                "{}",
                adapter.name
            );
        }
        // Anything SCV sets must survive the removal pass.
        for (variable, _) in adapter
            .home_environment
            .iter()
            .chain(adapter.fixed_environment)
        {
            assert!(!variable.ends_with("_API_KEY"), "{variable}");
        }
        // Stored credentials live inside the directory SCV points the CLI at.
        for store in [
            match adapter.status {
                Status::Stored(store) => Some(store),
                Status::Command(_) | Status::Unchecked => None,
            },
            match adapter.logout {
                Logout::Stored(store) => Some(store),
                Logout::Command(_) | Logout::Interactive { .. } => None,
            },
            match adapter.login {
                Login::ApiKey(store) => Some(store),
                _ => None,
            },
        ]
        .into_iter()
        .flatten()
        {
            let claude_private_home = matches!(
                store,
                KeyStore::ClaudeApiKey { .. } | KeyStore::ClaudeAccount { .. }
            );
            let paths = match store {
                KeyStore::ClaudeApiKey { key, credentials } => vec![key, credentials],
                KeyStore::ClaudeAccount {
                    credentials,
                    config,
                } => vec![credentials, config],
                KeyStore::Grok { auth, config } => vec![auth, config],
                KeyStore::DshRefs { path, .. } => vec![path],
                KeyStore::Pi { dir } => vec![dir],
                // The nested SCV's `SCV_HOME` is the agent home itself.
                KeyStore::Scv { .. } => vec![],
            };
            for path in paths {
                assert!(
                    claude_private_home
                        || adapter
                            .home_environment
                            .iter()
                            .any(|(_, home)| !home.is_empty() && path.starts_with(home)),
                    "{}: {path}",
                    adapter.name
                );
            }
        }
    }
}

#[test]
fn status_summaries_never_echo_accounts_or_keys() {
    assert_eq!(
        summarize_status(
            StatusSummary::CodexText,
            true,
            "Logged in using an API key - sk-proj-***abcd"
        ),
        "signed in (API key)"
    );
    assert_eq!(
        summarize_status(StatusSummary::CodexText, true, "Logged in using ChatGPT"),
        "signed in (ChatGPT account)"
    );
    assert_eq!(
        summarize_status(StatusSummary::CodexText, false, "Not logged in"),
        "not signed in"
    );
    for adapter in ADAPTERS {
        if let Status::Command(_) = adapter.status {
            assert_ne!(
                adapter.status_summary,
                StatusSummary::ExitStatus,
                "{}",
                adapter.name
            );
        }
    }
}

#[test]
fn removal_covers_every_adapter_and_generic_api_keys() {
    for removed in [
        "OPENAI_API_KEY",
        "CLAUDE_CONFIG_DIR",
        "GROK_HOME",
        "GROK_AUTH",
        "XAI_API_KEY",
        "DSH_HOME",
        "DSH_PERMISSION_MODE",
        "DEEPSEEK_BASE_URL",
        "PI_CODING_AGENT_DIR",
        "OPENROUTER_API_KEY",
        "SCV_CONFIG",
        "GEMINI_API_KEY",
        "GEMINI_CLI_HOME",
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "QWEN_HOME",
        "DASHSCOPE_API_KEY",
        "OPENCODE_CONFIG",
        "COPILOT_MODEL",
        "CURSOR_API_KEY",
        "AMP_URL",
        "KIMI_BASE_URL",
        "MOONSHOT_API_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
    ] {
        assert!(is_removed_agent_variable(OsStr::new(removed)), "{removed}");
    }
    // General GitHub credentials stay: other agents' git and gh use them.
    for kept in [
        "PATH",
        "HOME",
        "LANG",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GROKKING",
        "PIPX_HOME",
    ] {
        assert!(!is_removed_agent_variable(OsStr::new(kept)), "{kept}");
    }
}

#[test]
fn executables_resolve_from_per_user_directories_before_path() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join(".grok/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let name = "scv-test-agent-only-in-home";
    let executable = bin.join(name);
    std::fs::write(&executable, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let grok = adapter("grok").unwrap();
    let dirs = adapter_search_dirs(grok, dir.path());
    assert!(dirs.contains(&dir.path().join(".local/bin")));
    assert_eq!(
        resolve_agent_executable(name, &dirs),
        Some(executable.clone())
    );
    assert_eq!(resolve_agent_executable(name, &[]), None);
    // A per-user install wins over the same command on PATH.
    let shadow = bin.join("sh");
    std::fs::write(&shadow, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(resolve_agent_executable("sh", &dirs), Some(shadow));
    assert!(resolve_agent_executable("sh", &[]).is_some());
    assert_eq!(
        resolve_agent_executable(executable.to_str().unwrap(), &[]),
        Some(executable)
    );
}

#[test]
fn the_added_agents_fail_safe_where_their_integration_is_unverified() {
    let acp = |name: &str| adapter(name).and_then(|adapter| adapter.acp);
    for name in ["gemini", "qwen", "opencode", "kimi"] {
        assert!(acp(name).is_some(), "{name} speaks ACP");
    }
    for name in ["copilot", "cursor", "amp"] {
        assert!(acp(name).is_none(), "{name} has no verified ACP server");
    }
    // Kimi Code runs over ACP only, as `kimi acp`, and installs under
    // `~/.kimi-code/bin`.
    assert!(!adapter("kimi").unwrap().cli_turns);
    assert_eq!(acp_args(&acp("kimi").unwrap(), true), ["acp"]);
    assert_eq!(adapter("kimi").unwrap().search_dirs, [".kimi-code/bin"]);
    assert!(
        ADAPTERS
            .iter()
            .filter(|adapter| adapter.name != "kimi")
            .all(|adapter| adapter.cli_turns)
    );
    // SCV cannot check their sign-ins, so it never hides them for it.
    for name in [
        "gemini", "qwen", "opencode", "copilot", "cursor", "amp", "kimi",
    ] {
        assert_eq!(adapter(name).unwrap().status, Status::Unchecked, "{name}");
        assert_eq!(adapter(name).unwrap().auth, AgentAuth::None, "{name}");
    }
    // Devin is a cloud service with no local CLI or ACP server.
    assert!(adapter("devin").is_none());
    let gemini = acp("gemini").unwrap();
    assert_eq!(acp_args(&gemini, true), ["--yolo", "--experimental-acp"]);
    assert_eq!(acp_args(&gemini, false), ["--experimental-acp"]);
    assert_eq!(acp_args(&acp("opencode").unwrap(), true), ["acp"]);
    assert_eq!(
        OutputFormat::GeminiStreamJson.args(),
        ["--output-format", "stream-json"]
    );
    assert_eq!(OutputFormat::AmpStreamJson.args(), ["--stream-json"]);
    assert!(OutputFormat::CursorStreamJson.claude_like());
    assert!(!OutputFormat::GeminiStreamJson.claude_like());
    // Each prompt-flag CLI takes the prompt last, as one argument.
    assert_eq!(adapter("gemini").unwrap().prompt_args, ["-p"]);
    assert_eq!(adapter("amp").unwrap().prompt_args, ["-x"]);
    assert_eq!(adapter("cursor").unwrap().args, ["-p"]);
    assert_eq!(adapter("opencode").unwrap().args, ["run"]);
}
