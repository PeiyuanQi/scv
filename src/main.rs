use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use scv_server::{ApprovalPolicy, ConfigOverrides};
use scv_tui::LaunchOptions;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

#[derive(Parser)]
#[command(name = "scv", version, about = "SCV — Search, Construct, Verify")]
struct Cli {
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
    Config { #[command(subcommand)] command: ConfigCommand },
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
    Start { #[arg(long, value_name = "PATH", default_value = ".")] workspace: PathBuf },
    Stop,
    Restart { #[arg(long, value_name = "PATH", default_value = ".")] workspace: PathBuf },
    Status,
    /// Run the authoritative server.
    Server {
        /// Speak newline-delimited JSON over stdin/stdout.
        #[arg(long, default_value_t = true)]
        stdio: bool,
    },
    /// Connect a SCV workspace to a WeChat ClawBot/iLink account.
    Clawbot {
        #[command(subcommand)]
        command: Option<ClawbotCommand>,
        #[arg(long, default_value = "https://ilinkai.weixin.qq.com")]
        base_url: String,
    },
    /// Backwards-compatible alias for `clawbot login`.
    ClawbotLogin {
        /// Login API base URL.
        #[arg(long, default_value = "https://ilinkai.weixin.qq.com")]
        login_url: String,
    },
}
#[derive(Subcommand)]
enum ConfigCommand { Init }

#[derive(Subcommand)]
enum ClawbotCommand {
    Login {
        #[arg(long, default_value = "default")]
        account: String,
        #[arg(long, default_value = "https://ilinkai.weixin.qq.com")]
        login_url: String,
    },
    Run {
        #[arg(long, default_value = "default")]
        account: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    Status {
        #[arg(long, default_value = "default")]
        account: String,
    },
    Logout {
        #[arg(long, default_value = "default")]
        account: String,
    },
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
    let launch = LaunchOptions {
        model: cli.model.clone(),
        provider: cli.provider.clone(),
        base_url: cli.base_url.clone(),
        approval_policy: cli
            .approval_policy
            .map(|value| value.to_possible_value().unwrap().get_name().to_owned()),
    };
    match cli.command.unwrap_or(Command::Tui) {
        Command::Config { command: ConfigCommand::Init } => { let path = scv_server::init_user_config()?; println!("Created configuration at {}", path.display()); Ok(()) },
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
        Command::Run { workspace } => run_daemon(&workspace, ConfigOverrides {
            provider: cli.provider,
            model: cli.model,
            base_url: cli.base_url,
            approval_policy: cli.approval_policy.map(Into::into),
            no_tools: false,
        }).await,
        Command::Start { workspace } => daemon_control("start", Some(&workspace)),
        Command::Stop => daemon_control("stop", None),
        Command::Restart { workspace } => daemon_control("restart", Some(&workspace)),
        Command::Status => daemon_control("status", None),
        Command::Clawbot { command, base_url } => match command.unwrap_or(ClawbotCommand::Login { account: "default".into(), login_url: base_url }) {
            ClawbotCommand::Login { account, login_url } => scv_clawbot::login(&login_url, &account).await,
            ClawbotCommand::Run { account, workspace } => clawbot_run(&account, &workspace).await,
            ClawbotCommand::Status { account } => clawbot_status(&account),
            ClawbotCommand::Logout { account } => { scv_clawbot::state::remove(&account)?; println!("Removed SCV ClawBot account {account:?}."); Ok(()) }
        },
        Command::ClawbotLogin { login_url } => scv_clawbot::login(&login_url, "default").await,
    }
}

fn service_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(home.join(".config/systemd/user/scv.service"))
}

fn daemon_control(action: &str, workspace: Option<&Path>) -> Result<()> {
    if let Some(workspace) = workspace {
        let workspace = std::fs::canonicalize(workspace).context("resolve daemon workspace")?;
        let path = service_path()?;
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let binary = std::env::current_exe()?.display().to_string();
        let unit = format!("[Unit]\nDescription=SCV agent daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nWorkingDirectory={}\nExecStart={} run --workspace {}\nRestart=on-failure\nRestartSec=3\nEnvironment=RUST_LOG=info\n\n[Install]\nWantedBy=default.target\n", workspace.display(), binary, workspace.display());
        std::fs::write(path, unit).context("write SCV systemd unit")?;
    }
    let status = ProcessCommand::new("systemctl").args(["--user", "daemon-reload"]).status().context("run systemctl")?;
    if !status.success() { bail!("systemctl daemon-reload failed"); }
    let (verb, extra) = if action == "start" { ("enable", vec!["--now"]) } else { (action, Vec::new()) };
    let mut command = ProcessCommand::new("systemctl");
    command.args(["--user", verb]);
    command.args(extra);
    command.arg("scv.service");
    let status = command.status().context("run systemctl")?;
    if !status.success() { bail!("systemctl {action} scv.service failed"); }
    Ok(())
}

async fn run_daemon(workspace: &Path, overrides: ConfigOverrides) -> Result<()> {
    let socket = scv_server::default_socket_path()?;
    std::env::set_current_dir(workspace).with_context(|| format!("change to daemon workspace {}", workspace.display()))?;
    scv_server::run_socket(&socket, overrides).await
}

async fn clawbot_run(account: &str, workspace: &Path) -> Result<()> {
    let credentials = scv_clawbot::state::account(account)?.ok_or_else(|| anyhow!("ClawBot account {account:?} is not logged in; run `scv clawbot login --account {account}`"))?;
    let workspace = std::fs::canonicalize(workspace).context("resolve ClawBot workspace")?;
    scv_clawbot::run(&credentials.token, &credentials.base_url, account, &workspace).await
}

fn clawbot_status(account: &str) -> Result<()> {
    match scv_clawbot::state::account(account)? {
        Some(value) => println!("ClawBot account {account:?} is logged in at {}.", value.base_url),
        None => println!("ClawBot account {account:?} is not logged in."),
    }
    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
