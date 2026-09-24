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
            assert!(adapter.conversation_files.is_some(), "{}", adapter.name);
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
                Status::Command(_) => None,
            },
            match adapter.logout {
                Logout::Stored(store) => Some(store),
                Logout::Command(_) => None,
            },
            match adapter.login {
                Login::ApiKey(store) => Some(store),
                _ => None,
            },
        ]
        .into_iter()
        .flatten()
        {
            let paths = match store {
                KeyStore::Grok { auth, config } => vec![auth, config],
                KeyStore::DshRefs { path, .. } => vec![path],
                KeyStore::Pi { dir } => vec![dir],
                // The nested SCV's `SCV_HOME` is the agent home itself.
                KeyStore::Scv { .. } => vec![],
            };
            for path in paths {
                assert!(
                    adapter
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
    let claude = r#"{"loggedIn":true,"authMethod":"claude.ai","email":"me@example.com","orgName":"me@example.com's Organization","subscriptionType":"max"}"#;
    assert_eq!(
        summarize_status(StatusSummary::ClaudeJson, true, claude),
        "signed in (Claude account, max)"
    );
    assert_eq!(
        summarize_status(
            StatusSummary::ClaudeJson,
            true,
            r#"{"loggedIn":true,"authMethod":"api_key","subscriptionType":"me@example.com"}"#
        ),
        "signed in (API key)"
    );
    assert_eq!(
        summarize_status(StatusSummary::ClaudeJson, false, r#"{"loggedIn":false}"#),
        "not signed in"
    );
    assert_eq!(
        summarize_status(
            StatusSummary::ClaudeJson,
            true,
            "{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}\n\nsome stderr"
        ),
        "signed in (Claude account)"
    );
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
    ] {
        assert!(is_removed_agent_variable(OsStr::new(removed)), "{removed}");
    }
    for kept in ["PATH", "HOME", "LANG", "GH_TOKEN", "GROKKING", "PIPX_HOME"] {
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
