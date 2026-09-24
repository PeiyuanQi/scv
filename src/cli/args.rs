//! The `scv` command line: every subcommand, flag, and value type clap parses.

use clap::{Parser, Subcommand, ValueEnum};
use scv_protocol::RemoteTools;
use std::path::PathBuf;

use super::common::ApprovalArg;

#[derive(Parser)]
#[command(name = "scv", version, about = "SCV — Search, Construct, Verify")]
pub(crate) struct Cli {
    /// Isolated SCV instance root. `SCV_HOME` remains supported for scripts.
    #[arg(long, global = true, value_name = "PATH", env = "SCV_HOME")]
    pub(crate) scv_home: Option<PathBuf>,
    /// Explicit configuration file for this SCV instance.
    #[arg(long, global = true, value_name = "PATH", env = "SCV_CONFIG")]
    pub(crate) config_path: Option<PathBuf>,
    #[arg(long, global = true)]
    pub(crate) model: Option<String>,
    #[arg(long, global = true)]
    pub(crate) provider: Option<String>,
    #[arg(long, global = true)]
    pub(crate) base_url: Option<String>,
    #[arg(long, global = true, value_enum)]
    pub(crate) approval_policy: Option<ApprovalArg>,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Create, locate, and inspect this SCV instance's configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Start the interactive terminal client (the default).
    Tui,
    /// Run one prompt without the terminal UI.
    Exec {
        prompt: String,
        /// Approve filesystem, shell, and nested-agent tools for this run.
        #[arg(long)]
        yes: bool,
    },
    /// Run the single SCV daemon in the foreground.
    Run {
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: PathBuf,
    },
    /// Manage the single user-level SCV daemon.
    Start {
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: PathBuf,
        /// Verify that the current user can authenticate with sudo before starting.
        #[arg(long)]
        allow_sudo: bool,
    },
    Stop,
    Restart {
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: PathBuf,
        /// Verify that the current user can authenticate with sudo before restarting.
        #[arg(long)]
        allow_sudo: bool,
        /// Ask the running daemon to restart into the release installed at
        /// its path once the delegation that asks (if any) has finished and
        /// its report is stored and no owner message is being answered. The
        /// new release announces itself in that chat, or through `[notify]`.
        #[arg(long, conflicts_with_all = ["workspace", "allow_sudo"])]
        when_idle: bool,
        /// With --when-idle: the release you installed; the daemon checks it.
        #[arg(long, requires = "when_idle")]
        version: Option<String>,
        /// With --when-idle: the commit it was built from, for the announcement.
        #[arg(long, requires = "when_idle")]
        commit: Option<String>,
        /// With --when-idle: restart anyway after this many seconds [default: 600].
        #[arg(long, value_name = "SECONDS", requires = "when_idle")]
        max_wait: Option<u64>,
    },
    Status,
    /// Re-read component settings and saved accounts without restarting sessions.
    Reload,
    /// Run the authoritative server.
    Server {
        /// Newline-delimited JSON over stdin/stdout is the only mode; the
        /// flag is still accepted for the callers that pass it.
        #[arg(long, hide = true)]
        stdio: bool,
    },
    /// Install the latest SCV release from crates.io or a configured mirror.
    Update {
        /// Cargo registry index URL, overriding `[update].index_url` and `SCV_CARGO_INDEX_URL`.
        #[arg(long, value_name = "URL")]
        index_url: Option<String>,
    },
    /// Connect chat channels (WeChat, Feishu/Lark) to this SCV instance:
    /// sign accounts in, run them under the daemon, and check their
    /// connections.
    Channels {
        #[command(subcommand)]
        command: ChannelsCommand,
    },
    /// Sign in the agent CLIs SCV delegates to (Claude Code, Codex, Grok
    /// Build, DeepSeek Harness, pi).
    ///
    /// Each agent keeps its own credentials in SCV's private agent home
    /// (`<SCV home>/agents/<agent>`), separate from your personal login.
    Agents {
        #[command(subcommand)]
        command: AgentsCommand,
    },
    /// Print this binary's version and config layout as JSON; the daemon
    /// runs it on a newly installed release before restarting into it.
    #[command(hide = true)]
    BuildInfo,
    /// Carry out a planned restart outside the daemon: restart its unit,
    /// check the new release, and roll back when it fails.
    #[command(hide = true)]
    RestartWatchdog {
        #[arg(long, value_name = "PATH")]
        plan: PathBuf,
    },
}

#[derive(Subcommand)]
pub(crate) enum ConfigCommand {
    /// Create `config.toml` with a starter provider if it does not exist.
    Init,
    /// Show every path SCV uses, the settings in effect for a session started
    /// here and where each came from, and whether each credential is in
    /// place. Secrets are never shown.
    Show {
        /// Also list settings left at their defaults.
        #[arg(long)]
        all: bool,
    },
    /// Print the path of the settings file, `<SCV home>/config.toml`.
    Path,
}

#[derive(Subcommand)]
pub(crate) enum ChannelsCommand {
    /// Sign a channel account in by scanning the QR code it shows. For
    /// Feishu the scan creates a bot app; `--app-id` signs in an existing
    /// app instead.
    Login {
        #[arg(value_enum)]
        channel: ChannelArg,
        #[arg(long, default_value = "default")]
        account: String,
        /// WeChat: the iLink login API origin [default: https://ilinkai.weixin.qq.com].
        #[arg(long)]
        login_url: Option<String>,
        /// Feishu: sign in an existing app by its ID; the app secret is read
        /// from a hidden prompt or stdin, never an argument.
        #[arg(long, value_name = "CLI_ID")]
        app_id: Option<String>,
        /// Feishu, with --app-id: the owner's open_id for this app, the only
        /// sender remote tools can reach. Without it nobody gets tools.
        #[arg(long, value_name = "OPEN_ID", requires = "app_id")]
        owner_open_id: Option<String>,
    },
    /// Enable a signed-in account under the SCV daemon.
    Run {
        #[arg(value_enum)]
        channel: ChannelArg,
        #[arg(long, default_value = "default")]
        account: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Grant remote tools: `owner` gives the account's own owner full,
        /// auto-approved tools; `none` keeps every remote session tool-free.
        /// Omitted keeps the saved setting.
        #[arg(long, value_enum)]
        remote_tools: Option<RemoteToolsArg>,
    },
    /// Persistently disable a supervised account (credentials are retained).
    Stop {
        #[arg(value_enum)]
        channel: ChannelArg,
        #[arg(long, default_value = "default")]
        account: String,
    },
    /// Show channel accounts and their live connection state.
    Status {
        /// Only this channel [default: every channel].
        #[arg(value_enum)]
        channel: Option<ChannelArg>,
        /// Only this account [default: every account].
        #[arg(long)]
        account: Option<String>,
    },
    /// Stop an account and remove its local credentials and delivery state.
    Logout {
        #[arg(value_enum)]
        channel: ChannelArg,
        #[arg(long, default_value = "default")]
        account: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum ChannelArg {
    /// WeChat, through its ClawBot (iLink) bot.
    Wechat,
    /// Feishu, through a bot app.
    Feishu,
    /// Lark, Feishu's international edition: the `feishu` channel.
    Lark,
}

impl ChannelArg {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Wechat => scv_clawbot::CHANNEL,
            Self::Feishu | Self::Lark => scv_feishu::CHANNEL,
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Wechat => "WeChat",
            Self::Feishu => "Feishu",
            Self::Lark => "Lark",
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum AgentsCommand {
    /// Sign an agent in inside SCV's agent home: the agent's own login, or
    /// a key prompt for agents that use an API key.
    Login {
        #[arg(value_parser = agent_names())]
        agent: String,
        /// pi only: configure an OpenAI-compatible endpoint as pi's default,
        /// prompting for anything not given; the key is never an argument.
        #[arg(long)]
        openai_compatible: bool,
        /// With --openai-compatible: the endpoint's base URL (e.g. https://host/v1).
        #[arg(long, requires = "openai_compatible")]
        base_url: Option<String>,
        /// With --openai-compatible: the endpoint's wire protocol.
        #[arg(long, value_enum, requires = "openai_compatible")]
        wire_api: Option<WireApiArg>,
        /// With --openai-compatible: the default model id.
        #[arg(long, requires = "openai_compatible")]
        model: Option<String>,
        /// Extra arguments for the agent's login, after `--`
        /// (e.g. `scv agents login codex -- --device-auth`).
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// Show whether the delegated agents are signed in for SCV.
    Status {
        #[arg(value_parser = agent_names())]
        agent: Option<String>,
    },
    /// Remove an agent's SCV-private sign-in; your own login is untouched.
    Logout {
        #[arg(value_parser = agent_names())]
        agent: String,
    },
    /// List delegated agent runs of this SCV instance, from any SCV process.
    Ps {
        /// Include orphaned runs whose SCV process died, awaiting cleanup.
        #[arg(long)]
        all: bool,
    },
    /// Remove old delegated-conversation transcripts from SCV's adapter
    /// homes, keeping those a live conversation still uses.
    Gc {
        /// Only this agent's transcripts.
        #[arg(value_parser = agent_names())]
        agent: Option<String>,
        /// Remove transcripts last written at least this long ago: 30d, 12h,
        /// 90m, or seconds. Never less than an hour.
        #[arg(long, default_value = "30d", value_parser = scv_server::conversation_age)]
        older_than: std::time::Duration,
        /// Show what would be removed without removing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop a delegated run (its process group and tagged descendants).
    Kill {
        /// Handle from `scv agents ps`, such as codex-3f9a2c.
        #[arg(required_unless_present = "orphans", conflicts_with = "orphans")]
        handle: Option<String>,
        /// Stop every orphaned run now instead of at the daemon's next check.
        #[arg(long)]
        orphans: bool,
    },
    /// Copy a setup into SCV's agent home. `codex`: your own config.toml
    /// and an API-key auth.json (never a ChatGPT sign-in). `grok`: your own
    /// config.toml with its model profiles (never a `grok login` sign-in).
    /// `pi --from-scv-provider`: SCV's own OpenAI-compatible provider as pi's
    /// default.
    Import {
        #[arg(value_parser = agent_names())]
        agent: String,
        /// Agent home to copy from [default: codex $CODEX_HOME or ~/.codex;
        /// grok $GROK_HOME or ~/.grok].
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// pi: use SCV's active provider (base URL, model, and key).
        #[arg(long)]
        from_scv_provider: bool,
    },
}

fn agent_names() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(
        scv_server::adapters::ADAPTERS
            .iter()
            .map(|adapter| adapter.name),
    )
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum WireApiArg {
    Responses,
    Chat,
}

impl From<WireApiArg> for scv_server::WireApi {
    fn from(value: WireApiArg) -> Self {
        match value {
            WireApiArg::Responses => Self::Responses,
            WireApiArg::Chat => Self::ChatCompletions,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum RemoteToolsArg {
    None,
    Owner,
}

impl From<RemoteToolsArg> for RemoteTools {
    fn from(value: RemoteToolsArg) -> Self {
        match value {
            RemoteToolsArg::None => Self::None,
            RemoteToolsArg::Owner => Self::Owner,
        }
    }
}
