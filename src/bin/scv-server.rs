use anyhow::Result;
use clap::{Parser, ValueEnum};
use scv_server::{ApprovalPolicy, ConfigOverrides};

#[derive(Parser)]
#[command(name = "scv-server", version, about = "SCV stdio agent server")]
struct Cli {
    #[arg(long, default_value_t = true)]
    stdio: bool,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long, value_enum)]
    approval_policy: Option<ApprovalArg>,
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
    if !cli.stdio {
        anyhow::bail!("v0.1 supports only --stdio");
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
    scv_server::run_stdio(ConfigOverrides {
        provider: None,
        model: cli.model,
        base_url: cli.base_url,
        approval_policy: cli.approval_policy.map(Into::into),
    })
    .await
}
