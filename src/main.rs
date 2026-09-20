use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use scv_protocol::{DaemonCommand, DaemonStatus};
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
enum ConfigCommand {
    Init,
}

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
    /// Persistently disable a supervised account (credentials are retained).
    Stop {
        #[arg(long, default_value = "default")]
        account: String,
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
    apply_process_config(cli.scv_home.as_deref(), cli.config_path.as_deref(), &cwd)?;
    let launch = LaunchOptions {
        model: cli.model.clone(),
        provider: cli.provider.clone(),
        base_url: cli.base_url.clone(),
        approval_policy: cli
            .approval_policy
            .map(|value| value.to_possible_value().unwrap().get_name().to_owned()),
    };
    match cli.command.unwrap_or(Command::Tui) {
        Command::Config {
            command: ConfigCommand::Init,
        } => {
            let path = scv_server::init_user_config()?;
            println!("Created configuration at {}", path.display());
            Ok(())
        }
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
            workspace,
            allow_sudo,
        } => daemon_control(
            "restart",
            Some(&workspace),
            cli.approval_policy,
            cli.provider.as_deref(),
            cli.model.as_deref(),
            cli.base_url.as_deref(),
            allow_sudo,
        ),
        Command::Status => show_status(None).await,
        Command::Reload => {
            control(DaemonCommand::Reload).await?;
            println!("Component configuration reloaded.");
            Ok(())
        }
        Command::Update { index_url } => update_cli(&cwd, index_url),
        Command::Clawbot { command, base_url } => match command.unwrap_or(ClawbotCommand::Login {
            account: "default".into(),
            login_url: base_url,
        }) {
            ClawbotCommand::Login { account, login_url } => {
                clawbot_login(&login_url, &account).await
            }
            ClawbotCommand::Run { account, workspace } => clawbot_run(&account, &workspace).await,
            ClawbotCommand::Stop { account } => {
                control(DaemonCommand::ClawbotSet {
                    account,
                    enabled: false,
                    workspace: None,
                })
                .await?;
                println!("ClawBot account disabled and stopped.");
                Ok(())
            }
            ClawbotCommand::Status { account } => show_status(Some(&account)).await,
            ClawbotCommand::Logout { account } => {
                control(DaemonCommand::ClawbotLogout { account }).await?;
                println!("ClawBot stopped; local credentials and delivery state removed.");
                Ok(())
            }
        },
        Command::ClawbotLogin { login_url } => clawbot_login(&login_url, "default").await,
    }
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

async fn clawbot_run(account: &str, workspace: &Path) -> Result<()> {
    let workspace = std::fs::canonicalize(workspace).context("resolve ClawBot workspace")?;
    control(DaemonCommand::ClawbotSet {
        account: account.into(),
        enabled: true,
        workspace: Some(workspace.display().to_string()),
    })
    .await?;
    println!(
        "ClawBot enabled under the SCV daemon; use `scv clawbot status` for live connection state."
    );
    Ok(())
}

async fn control(command: DaemonCommand) -> Result<DaemonStatus> {
    scv_client::control(&scv_client::default_socket_path()?, command).await
}

async fn clawbot_login(login_url: &str, account: &str) -> Result<()> {
    scv_clawbot::login(login_url, account).await?;
    match control(DaemonCommand::Reload).await {
        Ok(_) => println!("Daemon refreshed; enabled accounts start automatically."),
        Err(_) => println!(
            "Credentials saved. The daemon will load enabled accounts at startup or its next refresh."
        ),
    }
    Ok(())
}

async fn show_status(account: Option<&str>) -> Result<()> {
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
    for health in status
        .components
        .iter()
        .filter(|h| account.is_none_or(|name| h.account == name))
    {
        // JSON escaping makes account identity and other untrusted strings terminal-safe.
        println!("{}", serde_json::to_string(health)?);
    }
    if status
        .components
        .iter()
        .all(|h| account.is_some_and(|name| h.account != name))
    {
        println!("No matching supervised components.");
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
