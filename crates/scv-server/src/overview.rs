//! `scv config show`: every path an SCV instance uses, the settings in effect
//! and where each came from, and whether each credential is in place. It
//! reads files only, takes no lock a running daemon needs, and never prints a
//! secret.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use scv_client::Layout;

use crate::config::{Config, ConfigOverrides};

/// Render the overview for a session started in `workspace`. With `all`,
/// settings left at their defaults are listed too.
pub fn render(workspace: &Path, overrides: &ConfigOverrides, all: bool) -> Result<String> {
    let layout = Layout::from_env()?;
    let home = crate::config::user_home_path()
        .map_or_else(|| layout.home().to_owned(), |home| home.to_owned());
    let layout = Layout::new(home);
    let mut out = String::new();
    let selected = if std::env::var_os("SCV_HOME").is_some() {
        "selected by SCV_HOME or --scv-home"
    } else {
        "the default instance"
    };
    writeln!(out, "SCV instance {} ({selected})", tilde(layout.home()))?;

    writeln!(out, "\nFiles")?;
    let mut files = vec![
        (layout.config(), "settings you edit"),
        (layout.credentials(), "channel sign-ins SCV writes"),
        (layout.agents(), "private homes of delegated agents"),
        (layout.skills(), "your skills"),
        (layout.state(), "runtime state SCV writes; not for editing"),
    ];
    if let Some(explicit) = std::env::var_os("SCV_CONFIG") {
        files.push((PathBuf::from(explicit), "extra settings from SCV_CONFIG"));
    }
    let project = workspace.join(".scv/config.toml");
    if project.is_file()
        && std::fs::canonicalize(&project).ok() != std::fs::canonicalize(layout.config()).ok()
    {
        files.push((project, "project settings for this directory"));
    }
    for (path, role) in &files {
        row(
            &mut out,
            &tilde(path),
            &format!("{} ({role})", describe(path)),
        )?;
    }
    let socket = layout.socket();
    let daemon = if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        "a daemon is listening"
    } else {
        "no daemon is listening"
    };
    row(&mut out, &tilde(&socket), daemon)?;
    if let Ok(unit) = crate::service_unit_path() {
        let state = if unit.exists() { "present" } else { "missing" };
        row(
            &mut out,
            &tilde(&unit),
            &format!("{state} (daemon service unit)"),
        )?;
    }

    writeln!(out, "\nSettings (value, then where it was set)")?;
    let mut active = None;
    match Config::settings_with_origins(Some(workspace), overrides) {
        Ok(settings) => {
            active = settings
                .iter()
                .find(|setting| setting.key == "provider.active")
                .map(|setting| setting.value.trim_matches('"').to_owned());
            let hidden = settings
                .iter()
                .filter(|setting| !all && setting.origin == "default")
                .count();
            for setting in settings
                .iter()
                .filter(|setting| all || setting.origin != "default")
            {
                writeln!(
                    out,
                    "  {} = {}  [{}]",
                    setting.key, setting.value, setting.origin
                )?;
            }
            if hidden > 0 {
                writeln!(
                    out,
                    "  ({hidden} more at their defaults; `scv config show --all` lists them)"
                )?;
            }
        }
        Err(error) => writeln!(out, "  unreadable: {}", safe_error(&error))?,
    }
    match Config::load(workspace, overrides.clone()) {
        Ok(config) => {
            let provider = &config.provider;
            let key = match (&provider.api_key, &provider.api_key_env) {
                (Some(key), _) if !key.trim().is_empty() => "api_key in the settings".to_owned(),
                (_, Some(variable)) if std::env::var_os(variable).is_some() => {
                    format!("${variable}, set here")
                }
                (_, Some(variable)) => format!("${variable}, which is not set in this shell"),
                _ => "none".to_owned(),
            };
            let name = active.map_or_else(
                || "[provider]".to_owned(),
                |name| format!("profile {name:?}"),
            );
            writeln!(
                out,
                "  In effect: {name}, model {:?} at {:?} ({} API), key from {key}",
                provider.model, provider.base_url, provider.wire_api
            )?;
        }
        Err(error) => writeln!(out, "  Invalid: {}", safe_error(&error))?,
    }

    writeln!(
        out,
        "\nChannels ([channels.<channel>.<account>] in config.toml)"
    )?;
    let mut any = false;
    let wechat = scv_clawbot::state::Store::new(&layout, scv_clawbot::CHANNEL);
    any |= channel(&mut out, layout.home(), &wechat, scv_clawbot::CHANNEL)?;
    let feishu = scv_feishu::state::Store::new(&layout, scv_feishu::CHANNEL);
    any |= channel(&mut out, layout.home(), &feishu, scv_feishu::CHANNEL)?;
    if !any {
        writeln!(out, "  none signed in; see `scv channels login`")?;
    }

    writeln!(
        out,
        "\nAgents ([agents.<name>] in config.toml; homes under {})",
        tilde(&layout.agents())
    )?;
    for adapter in scv_tools::adapters::ADAPTERS {
        let home = layout.agent_home(adapter.name);
        if !home.is_dir() {
            row(&mut out, adapter.name, "no home yet (created on first use)")?;
            continue;
        }
        let credentials: Vec<String> = adapter
            .credential_files
            .iter()
            .map(|file| format!("{file} {}", describe(&home.join(file))))
            .collect();
        row(&mut out, adapter.name, &credentials.join(", "))?;
        match crate::agent_import_status(adapter.name) {
            Ok(Some(line)) => writeln!(out, "  {:<32} {line}", "")?,
            Ok(None) => {}
            Err(error) => writeln!(
                out,
                "  {:<32} import record unreadable: {}",
                "",
                safe_error(&error)
            )?,
        }
    }

    let strays = layout.strays()?;
    if !strays.is_empty() {
        writeln!(out, "\nNot used by SCV")?;
        for stray in strays {
            let note = if stray.legacy {
                "left by an older SCV layout"
            } else {
                "not part of the SCV layout"
            };
            row(&mut out, &tilde(&stray.path), note)?;
        }
    }
    Ok(out)
}

/// The accounts of one channel; whether there were any.
fn channel<C: scv_channels::state::Credentials>(
    out: &mut String,
    home: &Path,
    store: &scv_channels::state::Store<C>,
    name: &str,
) -> Result<bool> {
    let mut accounts = store.account_names().unwrap_or_default();
    match store.configured_accounts() {
        Ok(configured) => accounts.extend(configured),
        Err(error) => writeln!(out, "  {name}: settings unreadable: {}", safe_error(&error))?,
    }
    accounts.sort();
    accounts.dedup();
    for account in &accounts {
        let (credentials, settings) = store.inspect(account);
        let settings = match settings {
            Ok(settings) => {
                let workspace = settings
                    .workspace
                    .as_deref()
                    .map_or_else(|| "the daemon's workspace".to_owned(), tilde);
                let tools = match settings.remote_tools {
                    scv_channels::state::RemoteTools::None => "no remote tools",
                    scv_channels::state::RemoteTools::Owner => "owner has remote tools",
                };
                let enabled = if settings.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                format!("{enabled}, {tools}, workspace {workspace}")
            }
            Err(error) => format!("invalid settings: {}", safe_error(&error)),
        };
        row(out, &format!("{name}:{account}"), &settings)?;
        let path = store.credentials_path(account)?;
        let credentials = match credentials {
            Ok(Some(_)) => describe(&path),
            Ok(None) => "missing: not signed in".into(),
            Err(_) => format!("{} but unreadable; log in again", describe(&path)),
        };
        let shown = path.strip_prefix(home).unwrap_or(&path);
        writeln!(
            out,
            "  {:<32} credentials {} {credentials}",
            "",
            shown.display()
        )?;
    }
    Ok(!accounts.is_empty())
}

fn row(out: &mut String, left: &str, right: &str) -> std::fmt::Result {
    writeln!(out, "  {left:<32} {right}")
}

/// Whether `path` exists and, on Unix, its permission bits.
fn describe(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).map_or(metadata.permissions().mode(), |target| {
                target.permissions().mode()
            }) & 0o777;
            let private = if mode & 0o077 == 0 {
                ""
            } else {
                ", readable by others"
            };
            format!("{mode:04o}{private}")
        }
        Err(_) => "missing".into(),
    }
}

/// `path` with the user's home shown as `~`.
fn tilde(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(&home).ok().map(Path::to_owned)) {
        Some(relative) if relative.as_os_str().is_empty() => "~".into(),
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

/// The outermost message of an error. SCV's own messages keep secrets out,
/// but a parser's inner detail may quote the file, which can hold keys.
fn safe_error(error: &anyhow::Error) -> String {
    error.to_string().replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_and_missing_files_are_described() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(describe(&file), "0600");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(describe(&file), "0644, readable by others");
        assert_eq!(describe(&dir.path().join("missing")), "missing");
    }
}
