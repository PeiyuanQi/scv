//! The native agent CLIs SCV can delegate to, one descriptor each.
//!
//! A descriptor is the whole integration: the default command line, where the
//! CLI keeps its state inside SCV's private agent home, which inherited
//! variables it must never see, and how `scv agents login|status|logout`
//! handle it. Adding an agent means adding one entry to [`ADAPTERS`].

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// How SCV signs an agent in, inside its private agent home.
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
    /// Copy SCV's own configuration: `scv agents import <name>`.
    Import,
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
    pub(crate) fn args(self) -> &'static [&'static str] {
        match self {
            Self::Text => &[],
            Self::ClaudeStreamJson => &["--output-format", "stream-json", "--verbose"],
            Self::CodexJsonl => &["--json"],
            Self::PiJson => &["--mode", "json"],
        }
    }
}

/// How SCV talks to an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// One CLI process per turn: the prompt is an argument and the reply is
    /// read from its output ([`OutputFormat`]), continued through [`Resume`].
    Process,
    /// A long-running `scv server --stdio` per conversation, driven over the
    /// SCV protocol: its tool approvals are relayed to the calling session
    /// and its events become progress.
    ScvProtocol,
}

/// How to start an agent's Agent Client Protocol (ACP) server: a long-running
/// process speaking JSON-RPC 2.0 over stdio, one conversation per ACP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpLaunch {
    /// The ACP server executable: the agent itself or its official adapter.
    pub command: &'static str,
    /// Its arguments; a `{full}` entry is replaced by `full_args` for
    /// `permissions = "full"` and dropped otherwise.
    pub(crate) args: &'static [&'static str],
    pub(crate) full_args: &'static [&'static str],
    /// The ACP session mode selected for `permissions = "full"`, for agents
    /// whose permission level is a session mode.
    pub full_mode: Option<&'static str>,
    /// Environment for the ACP server under `permissions = "full"`, for
    /// settings the server reads only from its environment.
    pub full_environment: &'static [(&'static str, &'static str)],
}

/// Expand `launch.args` for the configured permission level.
pub fn acp_args(launch: &AcpLaunch, full: bool) -> Vec<String> {
    let mut args = Vec::with_capacity(launch.args.len() + launch.full_args.len());
    for arg in launch.args {
        if *arg == "{full}" {
            if full {
                args.extend(launch.full_args.iter().map(|arg| (*arg).to_owned()));
            }
        } else {
            args.push((*arg).to_owned());
        }
    }
    args
}

/// How a CLI continues an earlier conversation. `{session}` in any argument
/// is replaced by the conversation's vendor session ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// Every call starts a fresh conversation.
    Unsupported,
    Supported {
        /// Starts a conversation under an ID SCV chooses. Empty when the CLI
        /// picks its own ID and reports it in its output (Codex).
        start: &'static [&'static str],
        /// Placed right after the fixed arguments when continuing: a
        /// subcommand such as Codex's `exec resume`.
        subcommand: &'static [&'static str],
        /// Options that continue the conversation.
        options: &'static [&'static str],
        /// Placed immediately before the prompt when continuing, for a CLI
        /// that takes the session ID as a positional argument.
        positional: &'static [&'static str],
    },
}

impl Resume {
    pub(crate) fn is_supported(self) -> bool {
        matches!(self, Self::Supported { .. })
    }

    /// Whether SCV chooses the vendor session ID when a conversation starts.
    pub(crate) fn assigns_id(self) -> bool {
        matches!(self, Self::Supported { start, .. } if !start.is_empty())
    }
}

/// Where a CLI keeps conversation transcripts inside its agent home:
/// files with `extension` anywhere below `dir`, named after their session ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConversationFiles {
    pub(crate) dir: &'static str,
    pub(crate) extension: &'static str,
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

/// A CLI's native credential file, relative to the agent home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStore {
    /// Grok: sign-ins from `grok login` in `auth` (a JSON object of entries),
    /// or an API key in the `config` profile of its default model.
    Grok {
        auth: &'static str,
        config: &'static str,
    },
    /// DeepSeek Harness `.credentials.yaml`, holding `refs.<variable>`.
    DshRefs {
        path: &'static str,
        variable: &'static str,
    },
    /// pi's agent directory: `auth.json`, plus the SCV-configured
    /// OpenAI-compatible endpoint in `models.json` and `settings.json`.
    Pi { dir: &'static str },
    /// A nested SCV's own `config.toml`, holding the provider copied from the
    /// user's SCV by `scv agents import scv`.
    Scv { config: &'static str },
}

#[derive(Debug, Clone, Copy)]
pub struct AdapterDescriptor {
    /// Short name: the tool is `agent_<name>` and the home `agents/<name>`.
    pub name: &'static str,
    /// Product name for messages and the tool description.
    pub product: &'static str,
    /// What this harness offers, as one factual clause for the tool
    /// description, so the model can choose between agents.
    pub(crate) offers: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Placed immediately before the prompt, for CLIs whose prompt is a flag
    /// value (`grok -p <prompt>`).
    pub prompt_args: &'static [&'static str],
    pub model_args: &'static [&'static str],
    pub effort_args: &'static [&'static str],
    /// Describes the `model` argument for the calling model.
    pub model_hint: &'static str,
    /// Variables pointing the CLI's state into the agent home, as paths
    /// relative to it (`""` is the home itself).
    pub home_environment: &'static [(&'static str, &'static str)],
    /// Fixed variables for every delegated run.
    pub fixed_environment: &'static [(&'static str, &'static str)],
    /// Credential, endpoint, and state-location variables no delegated agent
    /// inherits. A trailing `*` matches a prefix.
    pub(crate) removed_environment: &'static [&'static str],
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
    pub(crate) search_dirs: &'static [&'static str],
    pub login: Login,
    pub status: Status,
    /// How a [`Status::Command`] result is summarized.
    pub status_summary: StatusSummary,
    pub logout: Logout,
    /// What the CLI prints when SCV delegates to it.
    pub output: OutputFormat,
    /// How SCV continues a conversation with it, when it can.
    pub resume: Resume,
    /// Transcripts `scv agents gc` may remove; `None` when unknown.
    pub conversation_files: Option<ConversationFiles>,
    /// Files in the agent home that hold its sign-in or keys, relative to it,
    /// which `scv config show` reports without reading.
    pub credential_files: &'static [&'static str],
    /// How SCV talks to the agent.
    pub transport: Transport,
    /// Its ACP server, when it has a verified one. With `[agents.<name>]
    /// transport = "auto"` SCV prefers it over [`Transport::Process`] once the
    /// command is installed.
    pub acp: Option<AcpLaunch>,
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
const SCV_STORE: KeyStore = KeyStore::Scv {
    config: "config.toml",
};
const DSH_STORE: KeyStore = KeyStore::DshRefs {
    path: ".dsh/.credentials.yaml",
    variable: "DEEPSEEK_API_KEY",
};

pub const ADAPTERS: &[AdapterDescriptor] = &[
    AdapterDescriptor {
        name: "claude",
        product: "Claude Code",
        offers: "Anthropic's coding agent; it reads, edits, and runs code in a project and can search and fetch the web",
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
        // `--resume` in print mode keeps the original session ID.
        resume: Resume::Supported {
            start: &["--session-id", "{session}"],
            subcommand: &[],
            options: &["--resume", "{session}"],
            positional: &[],
        },
        conversation_files: Some(ConversationFiles {
            dir: ".claude/projects",
            extension: "jsonl",
        }),
        credential_files: &[".claude/.credentials.json"],
        transport: Transport::Process,
        // The official adapter from the ACP organisation (npm
        // @agentclientprotocol/claude-agent-acp), on the Claude Agent SDK.
        acp: Some(AcpLaunch {
            command: "claude-agent-acp",
            args: &[],
            full_args: &[],
            full_mode: Some("bypassPermissions"),
            full_environment: &[],
        }),
    },
    AdapterDescriptor {
        name: "codex",
        product: "Codex",
        offers: "OpenAI's coding agent; it reads, edits, and runs code in a project, with live web search under full permissions",
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
            "CODEX_CONFIG",
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
        // The thread ID arrives in `thread.started`; `exec resume` takes it
        // as a positional argument before the prompt.
        resume: Resume::Supported {
            start: &[],
            subcommand: &["resume"],
            options: &[],
            positional: &["{session}"],
        },
        conversation_files: Some(ConversationFiles {
            dir: "sessions",
            extension: "jsonl",
        }),
        credential_files: &["auth.json"],
        transport: Transport::Process,
        // The official adapter from the ACP organisation (npm
        // @agentclientprotocol/codex-acp). It reads `$CODEX_HOME/config.toml`
        // and takes no `-c` overrides; `CODEX_CONFIG` is its JSON form of
        // them, merged into every session, so full permissions keep live web
        // search without rewriting the imported config.
        acp: Some(AcpLaunch {
            command: "codex-acp",
            args: &[],
            full_args: &[],
            full_mode: Some("agent-full-access"),
            full_environment: &[("CODEX_CONFIG", r#"{"web_search":"live"}"#)],
        }),
    },
    AdapterDescriptor {
        name: "grok",
        product: "Grok Build",
        offers: "xAI's coding agent; it reads, edits, and runs code in a project and has live web and X search. Call it when another agent declined or refused a request, including a safety or guardrail refusal",
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
        status: Status::Stored(KeyStore::Grok {
            auth: ".grok/auth.json",
            config: ".grok/config.toml",
        }),
        status_summary: StatusSummary::ExitStatus,
        logout: Logout::Command(&["logout"]),
        // `--output-format json` exists but its success shape is unverified here.
        output: OutputFormat::Text,
        // Grok documents `--session-id` and `--resume`, but they cannot be
        // verified while it is signed out here.
        resume: Resume::Unsupported,
        conversation_files: None,
        credential_files: &[".grok/auth.json", ".grok/config.toml"],
        transport: Transport::Process,
        // Native: `grok agent [options] stdio`; options precede the mode.
        acp: Some(AcpLaunch {
            command: "grok",
            args: &["agent", "{full}", "stdio"],
            full_args: &["--always-approve"],
            full_mode: None,
            full_environment: &[],
        }),
    },
    AdapterDescriptor {
        name: "dsh",
        product: "DeepSeek Harness",
        offers: "a coding agent on DeepSeek models; it reads, edits, and runs code in a project",
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
        // Only its interactive profile documents `--resume`.
        resume: Resume::Unsupported,
        conversation_files: None,
        credential_files: &[".dsh/.credentials.yaml"],
        transport: Transport::Process,
        // Native: the shipped `acp` profile. `permissions = "full"` is the
        // `DSH_PERMISSION_MODE` variable above.
        acp: Some(AcpLaunch {
            command: "dsh",
            args: &["--profile", "acp"],
            full_args: &[],
            full_mode: None,
            full_environment: &[],
        }),
    },
    AdapterDescriptor {
        name: "pi",
        product: "pi",
        offers: "a minimal coding agent (read, write, edit, bash) that can run on SCV's own model endpoint; it has no web search",
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
        // `--session-id` uses the exact project session, creating it if missing.
        resume: Resume::Supported {
            start: &["--session-id", "{session}"],
            subcommand: &[],
            options: &["--session-id", "{session}"],
            positional: &[],
        },
        conversation_files: Some(ConversationFiles {
            dir: ".pi/agent/sessions",
            extension: "jsonl",
        }),
        credential_files: &[".pi/agent/auth.json", ".pi/agent/models.json"],
        transport: Transport::Process,
        // Only a community ACP adapter exists.
        acp: None,
    },
    AdapterDescriptor {
        name: "scv",
        product: "SCV",
        offers: "a nested SCV session with its own context and tools; suited to a self-contained sub-task kept out of this conversation's context, or work in another project",
        command: "scv",
        args: &["server", "--stdio"],
        prompt_args: &[],
        // A model is chosen per conversation through `session.start`.
        model_args: &[],
        effort_args: &[],
        model_hint: "Model ID for the nested SCV's provider; applies to a new conversation only.",
        // `SCV_HOME` already points at the agent home, where the nested
        // SCV keeps its config, skills, and its own delegations.
        home_environment: &[],
        fixed_environment: &[],
        removed_environment: &[],
        // Its tool approvals are relayed to the calling session instead.
        full_permission_args: &[],
        full_permission_environment: &[],
        // Where `cargo install` puts `scv`; a user service's PATH omits it.
        search_dirs: &[".cargo/bin"],
        login: Login::Import,
        status: Status::Stored(SCV_STORE),
        status_summary: StatusSummary::ExitStatus,
        logout: Logout::Stored(SCV_STORE),
        output: OutputFormat::Text,
        resume: Resume::Unsupported,
        conversation_files: None,
        credential_files: &["config.toml"],
        transport: Transport::ScvProtocol,
        acp: None,
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
            // The first JSON value; anything after it (such as stderr) is ignored.
            let first = serde_json::Deserializer::from_str(output)
                .into_iter::<serde_json::Value>()
                .next();
            let Some(Ok(value)) = first else {
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
mod tests;
