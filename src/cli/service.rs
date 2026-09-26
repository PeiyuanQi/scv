//! `scv start|stop|restart`: the systemd user unit that runs the daemon, and
//! the sudo expectation checked before starting it.

use anyhow::{Context, Result, bail};
use clap::ValueEnum as _;
use scv_client::Layout;
use scv_server::config::ConfigOverrides;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use super::common::ApprovalArg;

/// The command-line settings `scv start` and `scv restart` write into the
/// unit's `scv run` command.
pub(crate) struct Flags<'a> {
    pub(crate) approval_policy: Option<ApprovalArg>,
    pub(crate) provider: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) base_url: Option<&'a str>,
}

/// Start, stop, or restart the instance's systemd user unit, writing the
/// unit first when a `workspace` is given.
pub(crate) fn daemon_control(
    layout: &Layout,
    overrides: &ConfigOverrides,
    action: &str,
    workspace: Option<&Path>,
    flags: &Flags<'_>,
    allow_sudo: bool,
) -> Result<()> {
    if action == "start" || action == "restart" {
        ensure_sudo_expectation(allow_sudo)?;
    }
    let service = layout.service_name();
    if let Some(workspace) = workspace {
        let workspace = std::fs::canonicalize(workspace).context("resolve daemon workspace")?;
        stop_legacy_instance(layout, &service)?;
        let path = service_unit_path(layout)?;
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
            ("--provider", flags.provider),
            ("--model", flags.model),
            ("--base-url", flags.base_url),
        ] {
            if let Some(value) = value {
                command.push(flag.into());
                command.push(systemd_quote(std::ffi::OsStr::new(value)));
            }
        }
        if let Some(policy) = flags.approval_policy {
            command.push("--approval-policy".into());
            command.push(systemd_quote(std::ffi::OsStr::new(
                policy.to_possible_value().expect("value enum").get_name(),
            )));
        }
        // The unit selects the same instance and configuration file.
        let instance_environment = if layout.is_default() {
            String::new()
        } else {
            format!(
                "Environment=SCV_HOME={}\n",
                systemd_quote(layout.home().as_os_str())
            )
        };
        let config_environment = overrides
            .config_file
            .as_ref()
            .map(|config| {
                format!(
                    "Environment=SCV_CONFIG={}\n",
                    systemd_quote(config.as_os_str())
                )
            })
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
    command.arg(&service);
    let status = command.status().context("run systemctl")?;
    if !status.success() {
        bail!("systemctl {action} {service} failed");
    }
    Ok(())
}

/// Where `scv start` writes this instance's systemd user unit.
pub(crate) fn service_unit_path(layout: &Layout) -> Result<PathBuf> {
    let config = dirs::config_dir().context("cannot determine XDG config directory")?;
    Ok(config.join("systemd/user").join(layout.service_name()))
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
    scv_client::fs::replace_private(path, contents).context("install systemd unit")
}

fn stop_legacy_instance(layout: &Layout, service: &str) -> Result<()> {
    if service == "scv.service" || layout.is_default() {
        return Ok(());
    }
    let expected_home = format!("SCV_HOME={}", layout.home().display());
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
