//! The native agent CLIs SCV can delegate to, one descriptor each.
//!
//! A descriptor is the whole integration: the default command line, where the
//! CLI keeps its state inside SCV's private adapter home, which inherited
//! variables it must never see, and how `scv agents login|status|logout`
//! handle it. Adding an agent means adding one entry to [`ADAPTERS`].

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// How SCV signs an agent in, inside its private adapter home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    /// Run the CLI's own sign-in command.
    Command(&'static [&'static str]),
    /// Open the CLI interactively; `hint` names its in-app sign-in command.
    Interactive {
        args: &'static [&'static str],
        hint: &'static str,
    },
    /// Prompt for an API key and store it in the CLI's own credential file.
    ApiKey(KeyStore),
}

/// How SCV reports whether an agent is signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The CLI prints its own status and exits non-zero when signed out.
    Command(&'static [&'static str]),
    /// SCV inspects the CLI's credential file without printing secrets.
    Stored(KeyStore),
}

/// What a CLI prints on stdout when SCV runs it, and so how SCV reads its
/// reply, usage, and failure out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Plain text: stdout is the reply.
    Text,
    /// Claude Code `--output-format stream-json --verbose`: one JSON event per
    /// line, ending with a `result` event.
    ClaudeStreamJson,
    /// `codex exec --json`: JSON events per line; SCV also passes `-o <file>`
    /// so the final message survives an unparsable stream.
    CodexJsonl,
    /// pi `--mode json`: JSON events per line; the reply is the last
    /// assistant `message_end`.
    PiJson,
}

impl OutputFormat {
    /// Arguments that select this format, placed after the fixed arguments.
    pub fn args(self) -> &'static [&'static str] {
        match self {
            Self::Text => &[],
            Self::ClaudeStreamJson => &["--output-format", "stream-json", "--verbose"],
            Self::CodexJsonl => &["--json"],
            Self::PiJson => &["--mode", "json"],
        }
    }
}

/// How SCV condenses a CLI's own status output. The raw output names the
/// account (an email) or part of a key, so it is never printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSummary {
    /// `claude auth status` JSON: `loggedIn`, `authMethod`, `subscriptionType`.
    ClaudeJson,
    /// `codex login status` text: "Logged in using an API key" or "ChatGPT".
    CodexText,
    /// The exit status alone.
    ExitStatus,
}

/// How SCV signs an agent out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Logout {
    Command(&'static [&'static str]),
    /// SCV removes the credentials it can see in the CLI's own files.
    Stored(KeyStore),
}

/// A CLI's native credential file, relative to the adapter home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStore {
    /// A JSON object whose entries are stored sign-ins (Grok's `auth.json`).
    JsonEntries(&'static str),
    /// DeepSeek Harness `.credentials.yaml`, holding `refs.<variable>`.
    DshRefs {
        path: &'static str,
        variable: &'static str,
    },
    /// pi's agent directory: `auth.json`, plus the SCV-configured
    /// OpenAI-compatible endpoint in `models.json` and `settings.json`.
    Pi { dir: &'static str },
}

#[derive(Debug, Clone, Copy)]
pub struct AdapterDescriptor {
    /// Short name: the tool is `agent_<name>` and the home `adapters/<name>`.
    pub name: &'static str,
    /// Product name for messages.
    pub product: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Placed immediately before the prompt, for CLIs whose prompt is a flag
    /// value (`grok -p <prompt>`).
    pub prompt_args: &'static [&'static str],
    pub model_args: &'static [&'static str],
    pub effort_args: &'static [&'static str],
    /// Describes the `model` argument for the calling model.
    pub model_hint: &'static str,
    /// Variables pointing the CLI's state into the adapter home, as paths
    /// relative to it (`""` is the home itself).
    pub home_environment: &'static [(&'static str, &'static str)],
    /// Fixed variables for every delegated run.
    pub fixed_environment: &'static [(&'static str, &'static str)],
    /// Credential, endpoint, and state-location variables no delegated agent
    /// inherits. A trailing `*` matches a prefix.
    pub removed_environment: &'static [&'static str],
    /// Added after `args` when `[agents.<name>] permissions = "full"`: the
    /// CLI's own switches that turn off its approval prompts and sandbox and
    /// enable web search where the CLI gates it. Empty when the CLI has no
    /// permission system of its own.
    pub full_permission_args: &'static [&'static str],
    /// Variables set for `permissions = "full"`, for CLIs configured that way.
    pub full_permission_environment: &'static [(&'static str, &'static str)],
    /// Per-user install directories searched before `PATH`, relative to the
    /// user's home, as a login shell orders them. A user service's `PATH`
    /// omits them, so without this the daemon would miss or pick a different
    /// install than the user's shell.
    pub search_dirs: &'static [&'static str],
    pub login: Login,
    pub status: Status,
    /// How a [`Status::Command`] result is summarized.
    pub status_summary: StatusSummary,
    pub logout: Logout,
    /// What the CLI prints when SCV delegates to it.
    pub output: OutputFormat,
}

/// Directories every adapter searches before `PATH`, relative to the user's home.
const USER_BIN_DIRS: &[&str] = &[".local/bin"];

/// Removed from every agent regardless of adapter: SCV's own selectors and
/// cloud keys that name no single agent. Any variable ending in `_API_KEY`
/// is removed as well.
const COMMON_REMOVED_ENVIRONMENT: &[&str] = &[
    "SCV_CONFIG",
    "SCV_MODEL",
    "SCV_PROVIDER",
    "SCV_BASE_URL",
    "SCV_API_KEY_ENV",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
];

const PI_STORE: KeyStore = KeyStore::Pi { dir: ".pi/agent" };
const DSH_STORE: KeyStore = KeyStore::DshRefs {
    path: ".dsh/.credentials.yaml",
    variable: "DEEPSEEK_API_KEY",
};

pub const ADAPTERS: &[AdapterDescriptor] = &[
    AdapterDescriptor {
        name: "claude",
        product: "Claude Code",
        command: "claude",
        args: &["-p"],
        prompt_args: &[],
        model_args: &["--model", "{model}"],
        effort_args: &["--effort", "{effort}"],
        model_hint: "Claude model alias or ID, such as sonnet or opus.",
        home_environment: &[],
        fixed_environment: &[],
        removed_environment: &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
        ],
        // Also allows WebSearch and WebFetch without prompting.
        full_permission_args: &["--permission-mode", "bypassPermissions"],
        full_permission_environment: &[],
        search_dirs: &[],
        login: Login::Command(&["auth", "login"]),
        status: Status::Command(&["auth", "status"]),
        status_summary: StatusSummary::ClaudeJson,
        logout: Logout::Command(&["auth", "logout"]),
        output: OutputFormat::ClaudeStreamJson,
    },
    AdapterDescriptor {
        name: "codex",
        product: "Codex",
        command: "codex",
        args: &["exec"],
        prompt_args: &[],
        model_args: &["-m", "{model}"],
        effort_args: &["-c", "model_reasoning_effort=\"{effort}\""],
        model_hint: "OpenAI model ID from the Codex configuration; not a Claude alias.",
        home_environment: &[("CODEX_HOME", "")],
        fixed_environment: &[],
        removed_environment: &[
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_ORG_ID",
            "OPENAI_PROJECT_ID",
            "CODEX_API_KEY",
            "CODEX_BASE_URL",
        ],
        // `codex exec` has no `--search`; `web_search = "live"` is its config form.
        full_permission_args: &[
            "--dangerously-bypass-approvals-and-sandbox",
            "-c",
            "web_search=\"live\"",
        ],
        full_permission_environment: &[],
        search_dirs: &[],
        login: Login::Command(&["login"]),
        status: Status::Command(&["login", "status"]),
        status_summary: StatusSummary::CodexText,
        logout: Logout::Command(&["logout"]),
        output: OutputFormat::CodexJsonl,
    },
    AdapterDescriptor {
        name: "grok",
        product: "Grok Build",
        command: "grok",
        args: &[],
        prompt_args: &["-p"],
        model_args: &["-m", "{model}"],
        effort_args: &["--reasoning-effort", "{effort}"],
        model_hint: "xAI Grok model ID, such as grok-4.7.",
        home_environment: &[("GROK_HOME", ".grok")],
        fixed_environment: &[("GROK_DISABLE_AUTOUPDATER", "1")],
        removed_environment: &["GROK_*", "XAI_API_KEY"],
        // Web search is on unless `--disable-web-search` is passed.
        full_permission_args: &["--always-approve"],
        full_permission_environment: &[],
        search_dirs: &[".grok/bin"],
        login: Login::Command(&["login"]),
        status: Status::Stored(KeyStore::JsonEntries(".grok/auth.json")),
        status_summary: StatusSummary::ExitStatus,
        logout: Logout::Command(&["logout"]),
        // `--output-format json` exists but its success shape is unverified here.
        output: OutputFormat::Text,
    },
    AdapterDescriptor {
        name: "dsh",
        product: "DeepSeek Harness",
        command: "dsh",
        args: &["--profile", "headless"],
        prompt_args: &[],
        model_args: &[],
        effort_args: &[],
        model_hint: "Model ID in the form this agent's CLI accepts.",
        home_environment: &[("DSH_HOME", ".dsh")],
        fixed_environment: &[],
        removed_environment: &["DSH_*", "DEEPSEEK_API_KEY", "DEEPSEEK_BASE_URL"],
        // Bypasses its file sandbox and sets its approval policy to `never`.
        full_permission_args: &[],
        full_permission_environment: &[("DSH_PERMISSION_MODE", "danger-full-access")],
        search_dirs: &[],
        login: Login::ApiKey(DSH_STORE),
        status: Status::Stored(DSH_STORE),
        status_summary: StatusSummary::ExitStatus,
        logout: Logout::Stored(DSH_STORE),
        output: OutputFormat::Text,
    },
    AdapterDescriptor {
        name: "pi",
        product: "pi",
        command: "pi",
        args: &["-p"],
        prompt_args: &[],
        model_args: &["--model", "{model}"],
        effort_args: &["--thinking", "{effort}"],
        model_hint: "pi model pattern or provider/id; the SCV-configured endpoint is provider scv.",
        home_environment: &[("PI_CODING_AGENT_DIR", ".pi/agent")],
        fixed_environment: &[],
        removed_environment: &["PI_*"],
        // pi has no approval prompts or sandbox, and no built-in web search.
        full_permission_args: &[],
        full_permission_environment: &[],
        search_dirs: &[],
        login: Login::Interactive {
            args: &[],
            hint: "run /login and choose a provider, then /quit",
        },
        status: Status::Stored(PI_STORE),
        status_summary: StatusSummary::ExitStatus,
        logout: Logout::Stored(PI_STORE),
        output: OutputFormat::PiJson,
    },
];

/// The descriptor for `name`, such as `"codex"`.
pub fn adapter(name: &str) -> Option<&'static AdapterDescriptor> {
    ADAPTERS.iter().find(|adapter| adapter.name == name)
}

/// Whether a delegated agent must not inherit `variable`: SCV's selectors,
/// any `*_API_KEY`, and every adapter's credential and state variables, so
/// no agent sees another's credentials either.
pub fn is_removed_agent_variable(variable: &OsStr) -> bool {
    let Some(variable) = variable.to_str() else {
        return false;
    };
    variable.ends_with("_API_KEY")
        || COMMON_REMOVED_ENVIRONMENT.contains(&variable)
        || ADAPTERS
            .iter()
            .flat_map(|adapter| adapter.removed_environment)
            .any(|rule| match rule.strip_suffix('*') {
                Some(prefix) => variable.starts_with(prefix),
                None => variable == *rule,
            })
}

/// One line describing a CLI's own status result without echoing it: the raw
/// output names the signed-in account or part of a key.
pub fn summarize_status(summary: StatusSummary, succeeded: bool, output: &str) -> String {
    let signed_out = "not signed in".to_owned();
    match summary {
        StatusSummary::ClaudeJson => {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
                return if succeeded {
                    "signed in".into()
                } else {
                    signed_out
                };
            };
            if value.get("loggedIn").and_then(serde_json::Value::as_bool) != Some(true) {
                return signed_out;
            }
            let method = match value.get("authMethod").and_then(serde_json::Value::as_str) {
                Some("claude.ai") => "Claude account",
                Some("api_key" | "apiKey" | "console") => "API key",
                Some("oauth_token" | "oauthToken") => "OAuth token",
                _ => "other method",
            };
            match value
                .get("subscriptionType")
                .and_then(serde_json::Value::as_str)
                .filter(|plan| ["free", "pro", "max", "team", "enterprise"].contains(plan))
            {
                Some(plan) => format!("signed in ({method}, {plan})"),
                None => format!("signed in ({method})"),
            }
        }
        StatusSummary::CodexText => {
            let lower = output.to_ascii_lowercase();
            if !succeeded || lower.contains("not logged in") {
                signed_out
            } else if lower.contains("api key") {
                "signed in (API key)".into()
            } else if lower.contains("chatgpt") {
                "signed in (ChatGPT account)".into()
            } else {
                "signed in".into()
            }
        }
        StatusSummary::ExitStatus => {
            if succeeded {
                "signed in".into()
            } else {
                signed_out
            }
        }
    }
}

/// Resolve `command` in the per-user `search_dirs`, then on `PATH`. A command
/// containing a path separator is used as given.
pub fn resolve_agent_executable(command: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    if command.contains('/') {
        let path = Path::new(command);
        return path.is_file().then(|| path.to_path_buf());
    }
    std::env::join_paths(search_dirs)
        .ok()
        .and_then(|dirs| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            which::which_in(command, Some(dirs), cwd).ok()
        })
        .or_else(|| which::which(command).ok())
}

/// Absolute per-user search directories for `adapter` under `home`.
pub fn adapter_search_dirs(adapter: &AdapterDescriptor, home: &Path) -> Vec<PathBuf> {
    adapter
        .search_dirs
        .iter()
        .chain(USER_BIN_DIRS)
        .map(|dir| home.join(dir))
        .collect()
}

#[cfg(test)]
mod tests {
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
                let path = match store {
                    KeyStore::JsonEntries(path) | KeyStore::DshRefs { path, .. } => path,
                    KeyStore::Pi { dir } => dir,
                };
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
}
