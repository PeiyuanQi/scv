use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use scv_server::{ApprovalPolicy, ConfigOverrides};
use scv_tui::LaunchOptions;
use serde_json::json;
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
    /// Run the authoritative server.
    Server {
        /// Speak newline-delimited JSON over stdin/stdout.
        #[arg(long, default_value_t = true)]
        stdio: bool,
    },
    /// Connect a SCV workspace to a WeChat ClawBot/iLink account.
    ClawbotLogin {
        /// Login API base URL.
        #[arg(long, default_value = "https://ilinkai.weixin.qq.com")]
        login_url: String,
    },
    Clawbot {
        /// iLink bot bearer token (or set SCV_CLAWBOT_TOKEN).
        #[arg(long, env = "SCV_CLAWBOT_TOKEN")]
        token: String,
        #[arg(long, default_value = "https://ilinkai.weixin.qq.com")]
        base_url: String,
    },
}
#[derive(Subcommand)]
enum ConfigCommand { Init }

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
        Command::ClawbotLogin { login_url } => clawbot_login(&login_url).await,
        Command::Clawbot { token, base_url } => run_clawbot(&token, &base_url).await,
    }
}

async fn run_clawbot(token: &str, base_url: &str) -> Result<()> {
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
            let output = tokio::process::Command::new(std::env::current_exe()?).args(["exec", "--yes", text]).output().await?;
            let reply = String::from_utf8_lossy(&output.stdout).into_owned();
            client.post(format!("{base_url}/ilink/bot/sendmessage")).header("AuthorizationType", "ilink_bot_token").bearer_auth(token).header("X-WECHAT-UIN", "cGVvbg==").json(&json!({"msg":{"to_user_id":to_user_id,"client_id":Uuid::new_v4().to_string(),"message_type":2,"message_state":2,"context_token":context_token,"item_list":[{"type":1,"text_item":{"text":reply}}]},"base_info":{"channel_version":"1.0.2"}})).send().await?;
        }
    }
}

async fn clawbot_login(base: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let qr: serde_json::Value = client.post(format!("{base}/api/v1/wechat/qrcode")).send().await?.error_for_status()?.json().await?;
    let data = qr.get("data").unwrap_or(&qr);
    let url = data.get("qrcode_url").and_then(|v| v.as_str()).unwrap_or("");
    let code = data.get("qrcode").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("missing qrcode token"))?;
    println!("Scan this QR code URL in a browser:\n{url}");
    loop {
        let status: serde_json::Value = client.post(format!("{base}/api/v1/wechat/qrcode/status")).json(&json!({"qrcode":code})).send().await?.error_for_status()?.json().await?;
        let data = status.get("data").unwrap_or(&status);
        if data.get("status").and_then(|v| v.as_str()) == Some("confirmed") {
            let c = data.get("credentials").ok_or_else(|| anyhow::anyhow!("login confirmed without credentials"))?;
            println!("SCV_CLAWBOT_TOKEN={}", c.get("bot_token").and_then(|v| v.as_str()).unwrap_or(""));
            println!("CLAWBOT_BASE_URL={}", c.get("baseurl").and_then(|v| v.as_str()).unwrap_or(base)); return Ok(());
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
