use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use scv_server::{ApprovalPolicy, ConfigOverrides};
use scv_tui::LaunchOptions;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use uuid::Uuid;

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
enum ClawbotCommand { Login { #[arg(long, default_value = "https://ilinkai.weixin.qq.com")] login_url: String }, Logout }

#[derive(Debug, Serialize, Deserialize)]
struct ClawbotCredentials { token: String, base_url: String }

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
            })
            .await
        }
        Command::Run { workspace } => run_daemon(&workspace).await,
        Command::Start { workspace } => daemon_control("start", Some(&workspace)),
        Command::Stop => daemon_control("stop", None),
        Command::Restart { workspace } => daemon_control("restart", Some(&workspace)),
        Command::Status => daemon_control("status", None),
        Command::Clawbot { command, base_url } => match command.unwrap_or(ClawbotCommand::Login { login_url: base_url }) {
            ClawbotCommand::Login { login_url } => clawbot_login(&login_url).await,
            ClawbotCommand::Logout => { remove_credentials()?; println!("Removed SCV ClawBot credentials."); Ok(()) }
        },
        Command::ClawbotLogin { login_url } => clawbot_login(&login_url).await,
    }
}

fn credentials_path() -> Result<PathBuf> {
    let root = std::env::var_os("SCV_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|p| p.join(".scv"))).ok_or_else(|| anyhow!("cannot determine SCV_HOME"))?;
    Ok(root.join("clawbot.toml"))
}

fn read_credentials() -> Result<ClawbotCredentials> {
    let path = credentials_path()?;
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).context("parse ClawBot credentials")
}

fn write_credentials(credentials: &ClawbotCredentials) -> Result<()> {
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create SCV config directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .context("secure SCV config directory")?;
        }
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, toml::to_string(credentials).context("encode ClawBot credentials")?).context("write credentials")?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).context("secure credentials")?; }
    std::fs::rename(&temporary, &path).context("install credentials")
}

fn remove_credentials() -> Result<()> {
    let path = credentials_path()?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
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

async fn run_daemon(workspace: &Path) -> Result<()> {
    let credentials = read_credentials().context("ClawBot is not logged in; run `scv clawbot login`")?;
    run_clawbot(&credentials.token, &credentials.base_url, workspace).await
}

async fn run_clawbot(token: &str, base_url: &str, workspace: &Path) -> Result<()> {
    let client = reqwest::Client::new();
    let mut cursor = String::new();
    loop {
        let response: serde_json::Value = client.post(format!("{base_url}/ilink/bot/getupdates"))
            .header("AuthorizationType", "ilink_bot_token")
            .bearer_auth(token)
            .header("X-WECHAT-UIN", "cGVvbg==")
            .json(&json!({"get_updates_buf": cursor, "base_info": {"channel_version": "1.0.2"}}))
            .timeout(std::time::Duration::from_secs(45)).send().await?.json().await?;
        if let Some(next) = response.get("get_updates_buf").and_then(|v| v.as_str()) { cursor = next.into(); }
        for msg in response.get("msgs").and_then(|v| v.as_array()).into_iter().flatten() {
            let Some(text) = msg.get("item_list").and_then(|v| v.as_array()).and_then(|items| items.iter().find_map(|i| i.get("text_item")?.get("text")?.as_str())) else { continue };
            let Some(to_user_id) = msg.get("from_user_id").and_then(|v| v.as_str()) else { continue };
            let Some(context_token) = msg.get("context_token").and_then(|v| v.as_str()) else { continue };
            let output = tokio::process::Command::new(std::env::current_exe()?).current_dir(workspace).args(["exec", "--yes", text]).output().await?;
            let reply = String::from_utf8_lossy(&output.stdout).into_owned();
            client.post(format!("{base_url}/ilink/bot/sendmessage")).header("AuthorizationType", "ilink_bot_token").bearer_auth(token).header("X-WECHAT-UIN", "cGVvbg==").json(&json!({"msg":{"to_user_id":to_user_id,"client_id":Uuid::new_v4().to_string(),"message_type":2,"message_state":2,"context_token":context_token,"item_list":[{"type":1,"text_item":{"text":reply}}]},"base_info":{"channel_version":"1.0.2"}})).send().await?;
        }
    }
}

async fn clawbot_login(base: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let qr: serde_json::Value = client.get(format!("{base}/ilink/bot/get_bot_qrcode?bot_type=3")).send().await?.error_for_status()?.json().await?;
    let code = qr.get("qrcode").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("login response omitted qrcode"))?;
    let url = qr.get("qrcode_img_content").and_then(|v| v.as_str()).unwrap_or(code);
    println!("Scan this QR code in WeChat:\n{url}");
    loop {
        let status: serde_json::Value = client.get(format!("{base}/ilink/bot/get_qrcode_status")).query(&[("qrcode", code)]).send().await?.error_for_status()?.json().await?;
        if status.get("status").and_then(|v| v.as_str()) == Some("confirmed") {
            let token = status.get("bot_token").and_then(|v| v.as_str()).filter(|v| !v.is_empty()).ok_or_else(|| anyhow!("confirmed login omitted bot_token"))?;
            let base_url = status
                .get("baseurl")
                .or_else(|| status.get("base_url"))
                .and_then(|v| v.as_str())
                .unwrap_or(base);
            write_credentials(&ClawbotCredentials { token: token.to_owned(), base_url: base_url.to_owned() })?;
            println!("ClawBot login saved securely.");
            return Ok(())
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
