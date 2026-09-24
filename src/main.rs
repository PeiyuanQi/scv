use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use scv_protocol::{DaemonCommand, DaemonStatus, RemoteTools};
use scv_server::{ApprovalPolicy, ConfigOverrides};
use scv_tui::LaunchOptions;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

#[derive(Parser)]
#[command(name = "scv", version, about = "SCV — Search, Construct, Verify")]
struct Cli {
    /// Isolated SCV instance root. `SCV_HOME` remains supported for scripts.
    #[arg(long, global = true, value_name = "PATH", env = "SCV_HOME")]
    scv_home: Option<PathBuf>,
    /// Explicit configuration file for this SCV instance.
    #[arg(long, global = true, value_name = "PATH", env = "SCV_CONFIG")]
    config_path: Option<PathBuf>,
    #[arg(long, global = true)]
    model: Option<String>,
    #[arg(long, global = true)]
    provider: Option<String>,
    #[arg(long, global = true)]
    base_url: Option<String>,
    #[arg(long, global = true, value_enum)]
    approval_policy: Option<ApprovalArg>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
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
        /// Speak newline-delimited JSON over stdin/stdout.
        #[arg(long, default_value_t = true)]
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
enum ConfigCommand {
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
enum ChannelsCommand {
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
enum ChannelArg {
    /// WeChat, through its ClawBot (iLink) bot.
    Wechat,
    /// Feishu, through a bot app.
    Feishu,
    /// Lark, Feishu's international edition: the `feishu` channel.
    Lark,
}

impl ChannelArg {
    fn name(self) -> &'static str {
        match self {
            Self::Wechat => scv_clawbot::CHANNEL,
            Self::Feishu | Self::Lark => scv_feishu::CHANNEL,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Wechat => "WeChat",
            Self::Feishu => "Feishu",
            Self::Lark => "Lark",
        }
    }
}

#[derive(Subcommand)]
enum AgentsCommand {
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

/// Give the nested SCV behind `agent_scv` a copy of SCV's own provider.
fn import_scv_child() -> Result<()> {
    println!("Giving SCV's nested SCV (agent_scv) a copy of SCV's own provider");
    for line in scv_server::import_scv_from_scv_provider()? {
        println!("  {line}");
    }
    println!(
        "This is a copy: re-run after changing SCV's provider. Check with `scv agents status scv`."
    );
    Ok(())
}

fn agent_names() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(
        scv_server::adapters::ADAPTERS
            .iter()
            .map(|adapter| adapter.name),
    )
}

fn agent_descriptor(name: &str) -> Result<&'static scv_server::adapters::AdapterDescriptor> {
    scv_server::adapters::adapter(name).with_context(|| format!("unknown agent {name}"))
}

#[derive(Clone, Copy, ValueEnum)]
enum WireApiArg {
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
enum RemoteToolsArg {
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

#[derive(Clone, Copy, ValueEnum)]
enum ApprovalArg {
    OnRisk,
    Always,
    Never,
}

impl From<ApprovalArg> for ApprovalPolicy {
    fn from(value: ApprovalArg) -> Self {
        match value {
            ApprovalArg::OnRisk => Self::OnRisk,
            ApprovalArg::Always => Self::Always,
            ApprovalArg::Never => Self::Never,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    apply_process_config(cli.scv_home.as_deref(), cli.config_path.as_deref(), &cwd)?;
    let launch = LaunchOptions {
        model: cli.model.clone(),
        provider: cli.provider.clone(),
        base_url: cli.base_url.clone(),
        approval_policy: cli
            .approval_policy
            .map(|value| value.to_possible_value().unwrap().get_name().to_owned()),
    };
    let command = cli.command.unwrap_or(Command::Tui);
    refuse_nested_daemon_control(&command)?;
    match command {
        Command::Config { command } => match command {
            ConfigCommand::Init => {
                let path = scv_server::init_user_config()?;
                println!("Created configuration at {}", path.display());
                Ok(())
            }
            ConfigCommand::Show { all } => {
                let overrides = ConfigOverrides {
                    provider: cli.provider,
                    model: cli.model,
                    base_url: cli.base_url,
                    approval_policy: cli.approval_policy.map(Into::into),
                    no_tools: false,
                };
                print!("{}", scv_server::overview::render(&cwd, &overrides, all)?);
                Ok(())
            }
            ConfigCommand::Path => {
                println!("{}", scv_client::Layout::from_env()?.config().display());
                Ok(())
            }
        },
        Command::Tui => scv_tui::run_tui(&cwd, launch).await,
        Command::Exec { prompt, yes } => scv_tui::run_exec(&cwd, prompt, yes, launch).await,
        Command::Server { stdio } => {
            if !stdio {
                anyhow::bail!("v0.1 supports only --stdio");
            }
            init_tracing();
            scv_server::run_stdio(ConfigOverrides {
                provider: cli.provider,
                model: cli.model,
                base_url: cli.base_url,
                approval_policy: cli.approval_policy.map(Into::into),
                no_tools: false,
            })
            .await
        }
        Command::Run { workspace } => {
            // Daemon diagnostics (channel poll and delivery failures) go to
            // stderr, which the user service sends to the journal.
            init_tracing();
            run_daemon(
                &workspace,
                ConfigOverrides {
                    provider: cli.provider,
                    model: cli.model,
                    base_url: cli.base_url,
                    approval_policy: cli.approval_policy.map(Into::into),
                    no_tools: false,
                },
            )
            .await
        }
        Command::Start {
            workspace,
            allow_sudo,
        } => daemon_control(
            "start",
            Some(&workspace),
            cli.approval_policy,
            cli.provider.as_deref(),
            cli.model.as_deref(),
            cli.base_url.as_deref(),
            allow_sudo,
        ),
        Command::Stop => daemon_control("stop", None, None, None, None, None, false),
        Command::Restart {
            when_idle: true,
            version,
            commit,
            max_wait,
            ..
        } => restart_when_idle(version, commit, max_wait).await,
        Command::Restart {
            workspace,
            allow_sudo,
            ..
        } => daemon_control(
            "restart",
            Some(&workspace),
            cli.approval_policy,
            cli.provider.as_deref(),
            cli.model.as_deref(),
            cli.base_url.as_deref(),
            allow_sudo,
        ),
        Command::Status => show_status(None, None).await,
        Command::Reload => {
            control(DaemonCommand::Reload).await?;
            println!("Component configuration reloaded.");
            Ok(())
        }
        Command::Update { index_url } => update_cli(&cwd, index_url),
        Command::Channels { command } => channels(command).await,
        Command::Agents { command } => agents(command).await,
        Command::BuildInfo => {
            println!("{}", serde_json::to_string(&scv_server::build_info())?);
            Ok(())
        }
        Command::RestartWatchdog { plan } => {
            init_tracing();
            scv_server::restart_watchdog(&plan).await
        }
    }
}

/// A delegated agent (or an SCV it started) must not start, stop, replace, or
/// reconfigure daemons: that is the host's decision, and an SCV run from
/// inside a delegation would otherwise manage its parent.
fn refuse_nested_daemon_control(command: &Command) -> Result<()> {
    let depth = scv_server::delegation::current_depth();
    let lifecycle = match command {
        Command::Run { .. } => Some("run"),
        Command::Start { .. } => Some("start"),
        Command::Stop => Some("stop"),
        // Asking the daemon to restart itself when idle is how a delegated
        // release (the feature-flow deploy) hands over; the daemon decides.
        Command::Restart {
            when_idle: true, ..
        } => None,
        Command::Restart { .. } => Some("restart"),
        Command::RestartWatchdog { .. } => Some("restart-watchdog"),
        Command::Update { .. } => Some("update"),
        Command::Channels { .. } => Some("channels"),
        _ => None,
    };
    if depth > 0
        && let Some(action) = lifecycle
    {
        bail!(
            "`scv {action}` is refused inside a delegated agent run (delegation depth {depth}); \
             the host owner manages the daemon"
        );
    }
    Ok(())
}

/// Exit status of `scv restart --when-idle` when the daemon is not running
/// or predates it; the caller then restarts the unit itself.
const RESTART_UNSUPPORTED: i32 = 3;

async fn restart_when_idle(
    version: Option<String>,
    commit: Option<String>,
    max_wait: Option<u64>,
) -> Result<()> {
    let parent = std::env::var(scv_server::delegation::PARENT_VARIABLE)
        .ok()
        .filter(|chain| !chain.trim().is_empty());
    let status = match control(DaemonCommand::RestartWhenIdle {
        version,
        commit,
        parent,
        max_wait_seconds: max_wait,
    })
    .await
    {
        Ok(status) => status,
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("unknown variant") || message.contains("SCV daemon unavailable") {
                eprintln!("{message}");
                eprintln!(
                    "The daemon is not running or cannot schedule its own restart; restart its unit instead."
                );
                std::process::exit(RESTART_UNSUPPORTED);
            }
            return Err(error);
        }
    };
    let info = status
        .restart
        .context("the daemon did not report the scheduled restart")?;
    println!("Restart into v{} scheduled.", info.to_version);
    println!("{}", describe_restart(&info));
    Ok(())
}

/// One line on a scheduled restart: what it waits for and until when.
fn describe_restart(info: &scv_protocol::RestartInfo) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let left = info.deadline_unix_seconds.saturating_sub(now);
    let waiting = match &info.waiting_for {
        Some(what) => format!(
            "waiting for {what}, restarting anyway in {}m{:02}s",
            left / 60,
            left % 60
        ),
        None => "restarting now".into(),
    };
    let target = match &info.origin {
        Some(origin) => format!("the chat on {origin} that asked"),
        None => "the [notify] accounts".into(),
    };
    format!(
        "Restart into v{}: {waiting}; the outcome goes to {target}.",
        info.to_version
    )
}

fn update_cli(workspace: &Path, index_url: Option<String>) -> Result<()> {
    let configured = scv_server::update_index_url(workspace)?;
    let index_url = index_url
        .or_else(|| std::env::var("SCV_CARGO_INDEX_URL").ok())
        .or(configured);
    let mut cargo = ProcessCommand::new("cargo");
    cargo.args(["install", "--locked", "--force"]);
    if let Some(index_url) = index_url.as_deref() {
        cargo.args(["--index", index_url]);
        println!("Updating SCV from Cargo index {index_url}");
    } else {
        println!("Updating SCV from crates.io");
    }
    cargo.arg("scv-cli");
    let status = cargo.status().context("run cargo install for scv-cli")?;
    if !status.success() {
        bail!("SCV update failed while installing scv-cli");
    }

    let service = scv_server::service_name()?;
    let active = ProcessCommand::new("systemctl")
        .args(["--user", "is-active", "--quiet"])
        .arg(&service)
        .status()
        .is_ok_and(|status| status.success());
    if active {
        let status = ProcessCommand::new("systemctl")
            .args(["--user", "restart"])
            .arg(&service)
            .status()
            .context("restart SCV daemon after update")?;
        if !status.success() {
            bail!("SCV updated, but restarting {service} failed");
        }
        println!("SCV updated and the running daemon was restarted; clients can reconnect.");
    } else {
        println!("SCV updated. No active user daemon was found to restart.");
    }
    Ok(())
}

fn daemon_control(
    action: &str,
    workspace: Option<&Path>,
    approval_policy: Option<ApprovalArg>,
    provider: Option<&str>,
    model: Option<&str>,
    base_url: Option<&str>,
    allow_sudo: bool,
) -> Result<()> {
    if action == "start" || action == "restart" {
        ensure_sudo_expectation(allow_sudo)?;
    }
    if let Some(workspace) = workspace {
        let workspace = std::fs::canonicalize(workspace).context("resolve daemon workspace")?;
        let service = scv_server::service_name()?;
        stop_legacy_instance(&service)?;
        let path = scv_server::service_unit_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let binary = std::env::current_exe()?;
        let mut command = vec![
            systemd_quote(binary.as_os_str()),
            "run".into(),
            "--workspace".into(),
            systemd_quote(workspace.as_os_str()),
        ];
        for (flag, value) in [
            ("--provider", provider),
            ("--model", model),
            ("--base-url", base_url),
        ] {
            if let Some(value) = value {
                command.push(flag.into());
                command.push(systemd_quote(std::ffi::OsStr::new(value)));
            }
        }
        if let Some(policy) = approval_policy {
            command.push("--approval-policy".into());
            command.push(systemd_quote(std::ffi::OsStr::new(
                policy.to_possible_value().expect("value enum").get_name(),
            )));
        }
        let instance_environment = std::env::var_os("SCV_HOME")
            .map(|home| format!("Environment=SCV_HOME={}\n", systemd_quote(&home)))
            .unwrap_or_default();
        let config_environment = std::env::var_os("SCV_CONFIG")
            .map(|config| format!("Environment=SCV_CONFIG={}\n", systemd_quote(&config)))
            .unwrap_or_default();
        let unit = format!(
            "[Unit]\nDescription=SCV agent daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStart={}\nRestart=on-failure\nRestartSec=3\nEnvironment=RUST_LOG=info\n{}{}\n[Install]\nWantedBy=default.target\n",
            systemd_path(workspace.as_os_str()),
            command.join(" "),
            instance_environment,
            config_environment
        );
        write_atomic(&path, unit.as_bytes()).context("write SCV systemd unit")?;
    }
    let status = ProcessCommand::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("run systemctl")?;
    if !status.success() {
        bail!("systemctl daemon-reload failed");
    }
    let (verb, extra) = if action == "start" {
        ("enable", vec!["--now"])
    } else {
        (action, Vec::new())
    };
    let mut command = ProcessCommand::new("systemctl");
    command.args(["--user", verb]);
    command.args(extra);
    command.arg(scv_server::service_name()?);
    let status = command.status().context("run systemctl")?;
    if !status.success() {
        bail!("systemctl {action} {} failed", scv_server::service_name()?);
    }
    Ok(())
}

fn sudo_available() -> bool {
    ProcessCommand::new("sudo")
        .args(["-n", "-v"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn ensure_sudo_expectation(allow_sudo: bool) -> Result<()> {
    if allow_sudo {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            bail!(
                "`scv start --allow-sudo` requires an interactive terminal so sudo can authenticate the current user"
            );
        }
        let status = ProcessCommand::new("sudo")
            .arg("-v")
            .status()
            .context("check sudo authorization")?;
        if !status.success() {
            bail!(
                "sudo authorization failed; SCV cannot grant sudo access. Ask an administrator to add your user to the system sudo policy."
            );
        }
        return Ok(());
    }
    if sudo_available() {
        return Ok(());
    }
    let warning = "SCV is starting without verified sudo authorization. The daemon will run as your user, and commands requiring sudo may fail. Continue? [Y/n] ";
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!(
            "SCV has no verified sudo authorization. Re-run interactively to confirm continuing, or use `scv start --allow-sudo` to authenticate the current user's existing sudo rights."
        );
    }
    eprint!("{warning}");
    io::stderr().flush().context("flush sudo warning")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read sudo warning response")?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
        bail!("SCV start cancelled because sudo authorization was not verified.");
    }
    Ok(())
}

fn apply_process_config(home: Option<&Path>, config: Option<&Path>, cwd: &Path) -> Result<()> {
    if let Some(home) = home {
        let home = absolute_path(home, cwd);
        std::fs::create_dir_all(&home).context("create SCV instance home")?;
        let home = std::fs::canonicalize(home).context("resolve SCV instance home")?;
        // This runs before any SCV async work or child process is started.
        unsafe { std::env::set_var("SCV_HOME", home) };
    }
    if let Some(config) = config {
        let config = absolute_path(config, cwd);
        // This runs before any SCV async work or child process is started.
        unsafe { std::env::set_var("SCV_CONFIG", config) };
    }
    Ok(())
}

fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn systemd_quote(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    format!(
        "\"{}\"",
        value
            .replace('%', "%%")
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

/// Encode a path for a scalar systemd setting such as WorkingDirectory=.
/// Unlike ExecStart, scalar settings do not strip surrounding quotes.
fn systemd_path(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' => encoded.push_str("%%"),
            '\\' => encoded.push_str("\\\\"),
            '"' => encoded.push_str("\\\""),
            '\t' => encoded.push_str("\\x09"),
            '\n' => encoded.push_str("\\x0a"),
            ' ' => encoded.push_str("\\x20"),
            _ => encoded.push(character),
        }
    }
    encoded
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent"))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("create temporary systemd unit")?;
    temporary
        .write_all(contents)
        .context("write temporary systemd unit")?;
    temporary
        .as_file()
        .sync_all()
        .context("sync temporary systemd unit")?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .context("install systemd unit")
}

fn stop_legacy_instance(service: &str) -> Result<()> {
    if service == "scv.service" {
        return Ok(());
    }
    let Some(home) = std::env::var_os("SCV_HOME") else {
        return Ok(());
    };
    let expected_home = format!("SCV_HOME={}", PathBuf::from(home).display());
    let output = ProcessCommand::new("systemctl")
        .args([
            "--user",
            "show",
            "scv.service",
            "-p",
            "Environment",
            "--value",
        ])
        .output()
        .context("inspect legacy SCV service")?;
    if output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|entry| entry == expected_home)
    {
        let status = ProcessCommand::new("systemctl")
            .args(["--user", "disable", "--now", "scv.service"])
            .status()
            .context("stop legacy SCV service")?;
        if !status.success() {
            bail!("failed to stop legacy scv.service for this SCV profile");
        }
    }
    Ok(())
}

async fn run_daemon(workspace: &Path, overrides: ConfigOverrides) -> Result<()> {
    let socket = scv_server::default_socket_path()?;
    std::env::set_current_dir(workspace)
        .with_context(|| format!("change to daemon workspace {}", workspace.display()))?;
    scv_server::run_socket(&socket, overrides).await
}

async fn agents(command: AgentsCommand) -> Result<()> {
    use scv_server::adapters::{Login, Logout, Status};
    match command {
        AgentsCommand::Login {
            agent,
            openai_compatible,
            base_url,
            wire_api,
            model,
            extra,
        } => {
            let adapter = agent_descriptor(&agent)?;
            let name = adapter.name;
            if openai_compatible {
                if name != "pi" {
                    bail!(
                        "--openai-compatible configures pi; {name} signs in with `scv agents login {name}`"
                    );
                }
                let endpoint = scv_server::Endpoint {
                    base_url: match base_url {
                        Some(url) => url,
                        None => prompt_line("Base URL (e.g. https://host/v1)")?,
                    },
                    api: match wire_api {
                        Some(api) => api.into(),
                        None => match prompt_line("Wire API [responses/chat] (default responses)")?
                            .as_str()
                        {
                            "" | "responses" => scv_server::WireApi::Responses,
                            "chat" => scv_server::WireApi::ChatCompletions,
                            other => bail!("unknown wire API {other:?}; use responses or chat"),
                        },
                    },
                    model: match model {
                        Some(model) => model,
                        None => prompt_line("Default model id")?,
                    },
                };
                let key = scv_server::read_secret("API key (input hidden)")?;
                for line in scv_server::configure_pi_endpoint(&endpoint, &key)? {
                    println!("{line}");
                }
                println!("Check with `scv agents status pi`.");
                return Ok(());
            }
            println!(
                "Signing {name} in for SCV's agent_{name} tool (separate from your own {} login).",
                adapter.product
            );
            match adapter.login {
                Login::Command(args) => run_agent(name, args, &extra, "sign-in")?,
                Login::Interactive { args, hint } => {
                    println!("Opening {} in SCV's agent home: {hint}.", adapter.product);
                    run_agent(name, args, &extra, "sign-in")?;
                }
                Login::Import => {
                    if !extra.is_empty() {
                        bail!("{name} copies SCV's own configuration and takes no arguments");
                    }
                    import_scv_child()?;
                }
                Login::ApiKey(store) => {
                    if !extra.is_empty() {
                        bail!("{name} takes its API key from a prompt or stdin, not arguments");
                    }
                    let key = scv_server::read_secret(&format!(
                        "{} API key (input hidden)",
                        adapter.product
                    ))?;
                    for line in scv_server::store_agent_key(name, store, &key)? {
                        println!("{line}");
                    }
                }
            }
            println!(
                "Done. `scv agents status` shows the result; the daemon picks it up on the next call."
            );
            Ok(())
        }
        AgentsCommand::Status { agent } => {
            let selected: Vec<_> = match agent {
                Some(name) => vec![agent_descriptor(&name)?],
                None => scv_server::adapters::ADAPTERS.iter().collect(),
            };
            for adapter in selected {
                let name = adapter.name;
                println!("{name}:");
                let installed = scv_server::agent_executable(name)?;
                if installed.is_none() {
                    println!(
                        "  not installed ({:?} is not on PATH or in ~/.local/bin)",
                        adapter.command
                    );
                }
                match adapter.status {
                    // The agent's own status names the account (an email) or
                    // part of a key, so only a summary is printed; its own
                    // advice would also sign in the wrong home.
                    Status::Command(args) if installed.is_some() => {
                        match scv_server::agent_command(name)?
                            .args(args)
                            .stdin(std::process::Stdio::null())
                            .output()
                        {
                            Ok(output) => {
                                // Codex reports on stderr, Claude Code on stdout.
                                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                                text.push('\n');
                                text.push_str(&String::from_utf8_lossy(&output.stderr));
                                let summary = scv_server::adapters::summarize_status(
                                    adapter.status_summary,
                                    output.status.success(),
                                    &text,
                                );
                                println!("  {summary}");
                                if summary == "not signed in" {
                                    println!("  Sign in for SCV with: scv agents login {name}");
                                }
                            }
                            Err(error) => println!("  unavailable: {error}"),
                        }
                    }
                    Status::Command(_) => {}
                    Status::Stored(store) => {
                        let (ready, lines) = scv_server::agent_stored_status(name, store)?;
                        for line in lines {
                            println!("  {line}");
                        }
                        if !ready {
                            println!("  Sign in for SCV with: scv agents login {name}");
                        }
                    }
                }
                if let Some(line) = scv_server::agent_import_status(name)? {
                    println!("  {line}");
                }
            }
            Ok(())
        }
        AgentsCommand::Ps { all } => {
            let status = control(DaemonCommand::Delegations { all }).await?;
            print_delegations(&status.delegations.entries);
            Ok(())
        }
        AgentsCommand::Gc {
            agent,
            older_than,
            dry_run,
        } => {
            let reports = scv_server::collect_agent_garbage(agent.as_deref(), older_than, dry_run)?;
            if reports.is_empty() {
                println!("No agent keeps conversation transcripts yet.");
            }
            for (agent, report) in reports {
                let verb = if dry_run { "would remove" } else { "removed" };
                println!(
                    "{agent}: {verb} {} transcript(s), {:.1} MiB; kept {} in use",
                    report.removed.len(),
                    report.bytes as f64 / (1024.0 * 1024.0),
                    report.kept_live
                );
                if dry_run {
                    for path in &report.removed {
                        println!("  {}", path.display());
                    }
                }
            }
            Ok(())
        }
        AgentsCommand::Kill { handle, orphans } => {
            let status = control(DaemonCommand::DelegationKill { handle, orphans }).await?;
            if status.delegations.killed.is_empty() {
                println!("Nothing to stop.");
            } else {
                println!("Stopped: {}", status.delegations.killed.join(", "));
            }
            Ok(())
        }
        AgentsCommand::Import {
            agent,
            from,
            from_scv_provider,
        } => match agent.as_str() {
            "codex" => {
                if from_scv_provider {
                    bail!("--from-scv-provider applies to pi; codex imports your own Codex home");
                }
                let source = from
                    .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
                    .or_else(|| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
                    })
                    .context("cannot determine your Codex home; pass --from")?;
                println!("Importing Codex setup from {}", source.display());
                for line in scv_server::import_codex(&source)? {
                    println!("  {line}");
                }
                println!(
                    "This is a copy: re-run after changing your own Codex config. Check with `scv agents status codex`."
                );
                Ok(())
            }
            "grok" => {
                if from_scv_provider {
                    bail!("--from-scv-provider applies to pi; grok imports your own Grok home");
                }
                let source = from
                    .or_else(|| std::env::var_os("GROK_HOME").map(PathBuf::from))
                    .or_else(|| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".grok"))
                    })
                    .context("cannot determine your Grok home; pass --from")?;
                println!("Importing Grok setup from {}", source.display());
                for line in scv_server::import_grok(&source)? {
                    println!("  {line}");
                }
                println!(
                    "This is a copy: re-run after changing your own Grok config. Check with `scv agents status grok`."
                );
                Ok(())
            }
            "scv" => {
                if from.is_some() {
                    bail!("scv imports SCV's own provider; it takes no --from");
                }
                import_scv_child()
            }
            "pi" => {
                if !from_scv_provider || from.is_some() {
                    bail!(
                        "pi imports SCV's own provider: `scv agents import pi --from-scv-provider`"
                    );
                }
                println!("Pointing SCV's pi at SCV's own provider");
                for line in scv_server::import_pi_from_scv_provider()? {
                    println!("  {line}");
                }
                println!(
                    "This is a copy: re-run after changing SCV's provider. Check with `scv agents status pi`."
                );
                Ok(())
            }
            other => {
                bail!("{other} has nothing to import; sign it in with `scv agents login {other}`")
            }
        },
        AgentsCommand::Logout { agent } => {
            let adapter = agent_descriptor(&agent)?;
            match adapter.logout {
                Logout::Command(args) => run_agent(adapter.name, args, &[], "sign-out"),
                Logout::Stored(store) => {
                    for line in scv_server::remove_agent_credentials(adapter.name, store)? {
                        println!("{line}");
                    }
                    Ok(())
                }
            }
        }
    }
}

/// Run an agent's own command inside its SCV agent home.
fn run_agent(name: &str, args: &[&str], extra: &[String], action: &str) -> Result<()> {
    let status = scv_server::agent_command(name)?
        .args(args)
        .args(extra)
        .status()
        .with_context(|| format!("run {name} {action}"))?;
    if !status.success() {
        bail!("{name} {action} did not complete");
    }
    Ok(())
}

/// Read one non-secret line, from the terminal or piped stdin.
fn prompt_line(prompt: &str) -> Result<String> {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};
    if std::io::stdin().is_terminal() {
        eprint!("{prompt}: ");
        std::io::stderr().flush().ok();
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

async fn channels(command: ChannelsCommand) -> Result<()> {
    match command {
        ChannelsCommand::Login {
            channel,
            account,
            login_url,
            app_id,
            owner_open_id,
        } => {
            match channel {
                ChannelArg::Wechat => {
                    if app_id.is_some() {
                        bail!("--app-id and --owner-open-id are Feishu options");
                    }
                    let login_url =
                        login_url.unwrap_or_else(|| "https://ilinkai.weixin.qq.com".into());
                    scv_clawbot::login(&login_url, &account).await?;
                }
                ChannelArg::Feishu | ChannelArg::Lark => {
                    if login_url.is_some() {
                        bail!("--login-url is a WeChat option");
                    }
                    let brand = match channel {
                        ChannelArg::Lark => scv_feishu::state::Brand::Lark,
                        _ => scv_feishu::state::Brand::Feishu,
                    };
                    match app_id {
                        Some(app_id) => {
                            let secret = scv_server::read_secret(&format!(
                                "{} app secret (input hidden)",
                                brand.title()
                            ))?;
                            scv_feishu::login::login_existing(
                                &account,
                                &app_id,
                                &secret,
                                owner_open_id.as_deref(),
                                brand,
                            )
                            .await?;
                        }
                        None => scv_feishu::login::login(&account, brand).await?,
                    }
                }
            }
            reload_after_login().await
        }
        ChannelsCommand::Run {
            channel,
            account,
            workspace,
            remote_tools,
        } => channel_run(channel, &account, &workspace, remote_tools.map(Into::into)).await,
        ChannelsCommand::Stop { channel, account } => {
            control(DaemonCommand::ChannelSet {
                channel: channel.name().into(),
                account,
                enabled: false,
                workspace: None,
                remote_tools: None,
            })
            .await?;
            println!("{} account disabled and stopped.", channel.title());
            Ok(())
        }
        ChannelsCommand::Status { channel, account } => {
            show_status(channel.map(ChannelArg::name), account.as_deref()).await
        }
        ChannelsCommand::Logout { channel, account } => {
            control(DaemonCommand::ChannelLogout {
                channel: channel.name().into(),
                account,
            })
            .await?;
            println!(
                "{} account stopped; local credentials and delivery state removed.",
                channel.title()
            );
            Ok(())
        }
    }
}

async fn channel_run(
    channel: ChannelArg,
    account: &str,
    workspace: &Path,
    remote_tools: Option<RemoteTools>,
) -> Result<()> {
    let workspace = std::fs::canonicalize(workspace).context("resolve channel workspace")?;
    let status = control(DaemonCommand::ChannelSet {
        channel: channel.name().into(),
        account: account.into(),
        enabled: true,
        workspace: Some(workspace.display().to_string()),
        remote_tools,
    })
    .await?;
    println!(
        "{} account enabled under the SCV daemon; use `scv channels status {}` for live connection state.",
        channel.title(),
        channel.name()
    );
    if remote_tools == Some(RemoteTools::Owner) {
        // Report what the daemon applied: credentials without an owner ID
        // grant tools to nobody.
        let effective = status.components.iter().any(|health| {
            health.channel == channel.name()
                && health.account == account
                && health.remote_tools == RemoteTools::Owner
        });
        if effective {
            println!(
                "Remote tools: the account's own {} owner now runs every SCV tool without approval prompts.",
                channel.title()
            );
        } else {
            println!(
                "Warning: owner remote tools are saved but not active; the login lacks an owner ID. Remote sessions stay tool-free."
            );
        }
    }
    Ok(())
}

fn print_delegations(entries: &[scv_protocol::DelegationInfo]) {
    if entries.is_empty() {
        println!("No delegated agent runs.");
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    println!(
        "{:<16} {:<7} {:<13} {:<9} {:>8} {:>5} {:>7} {:>5}  CWD",
        "HANDLE", "AGENT", "CONVERSATION", "STATE", "PID", "PROCS", "AGE", "DEPTH"
    );
    for entry in entries {
        let age = now.saturating_sub(entry.started_unix_seconds);
        let conversation = match (&entry.conversation, entry.turn) {
            (Some(handle), Some(turn)) => format!("{handle} #{turn}"),
            (Some(handle), None) => handle.clone(),
            _ => "-".into(),
        };
        // Debug formatting escapes control characters in the untrusted path.
        println!(
            "{:<16} {:<7} {:<13} {:<9} {:>8} {:>5} {:>7} {:>5}  {:?}",
            entry.handle,
            entry.agent,
            conversation,
            if entry.orphaned {
                "orphaned"
            } else {
                "running"
            },
            entry.pid,
            entry.processes,
            format!("{}m{:02}s", age / 60, age % 60),
            entry.depth,
            entry.cwd,
        );
    }
}

async fn control(command: DaemonCommand) -> Result<DaemonStatus> {
    scv_client::control(&scv_client::default_socket_path()?, command).await
}

async fn reload_after_login() -> Result<()> {
    match control(DaemonCommand::Reload).await {
        Ok(_) => println!("Daemon refreshed; enabled accounts start automatically."),
        Err(_) => println!(
            "Credentials saved. The daemon will load enabled accounts at startup or its next refresh."
        ),
    }
    Ok(())
}

async fn show_status(channel: Option<&str>, account: Option<&str>) -> Result<()> {
    let status = match control(DaemonCommand::Status).await {
        Ok(status) => status,
        Err(error) => {
            println!("Daemon: unavailable; component connectivity is unknown.");
            return Err(error);
        }
    };
    println!(
        "Daemon: running, version {}, pid {}",
        status.version, status.pid
    );
    println!(
        "Delegations: {} running, {} orphaned runs stopped since the daemon started",
        status.delegations.active, status.delegations.reaped
    );
    if let Some(restart) = &status.restart {
        println!("{}", describe_restart(restart));
    }
    let matching: Vec<_> = status
        .components
        .iter()
        .filter(|h| channel.is_none_or(|name| h.channel == name))
        .filter(|h| account.is_none_or(|name| h.account == name))
        .collect();
    let enabled = matching.iter().filter(|health| health.enabled).count();
    let connected = matching
        .iter()
        .filter(|health| health.enabled && health.state == scv_protocol::ComponentState::Connected)
        .count();
    println!("Channels: {connected} of {enabled} enabled accounts connected");
    for health in &matching {
        // JSON escaping makes account identity and other untrusted strings terminal-safe.
        println!("{}", serde_json::to_string_pretty(health)?);
    }
    if matching.is_empty() {
        println!("No matching supervised components.");
    }
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // Colour only for a terminal: the user service sends stderr to the
    // journal, where escape codes would hide `WARN`/`ERROR` from searches.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .try_init();
}
