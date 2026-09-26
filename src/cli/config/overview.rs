//! `scv config show`: every path an SCV instance uses, the settings in effect
//! and where each came from, and whether each credential is in place. It
//! reads files only, takes no lock a running daemon needs, and never prints a
//! secret.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use scv_client::Layout;
use scv_server::config::{Config, ConfigOverrides};

use crate::cli::{agents::setup, service};

/// Render the overview for a session started in `workspace`. With `all`,
/// settings left at their defaults are listed too.
pub(crate) fn render(
    layout: &Layout,
    workspace: &Path,
    overrides: &ConfigOverrides,
    all: bool,
) -> Result<String> {
    let mut out = String::new();
    let selected = if layout.is_default() {
        "the default instance"
    } else {
        "selected by SCV_HOME or --scv-home"
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
    if let Some(explicit) = &overrides.config_file {
        files.push((explicit.clone(), "extra settings from SCV_CONFIG"));
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
    if let Ok(unit) = service::service_unit_path(layout) {
        let state = if unit.exists() { "present" } else { "missing" };
        row(
            &mut out,
            &tilde(&unit),
            &format!("{state} (daemon service unit)"),
        )?;
    }

    writeln!(out, "\nSettings (value, then where it was set)")?;
    let mut active = None;
    match Config::settings_with_origins(layout, Some(workspace), overrides) {
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
    match Config::load(layout, workspace, overrides.clone()) {
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
    // The raw file shows which media limits an account sets; the rest are
    // defaults SCV leaves out of it.
    let raw = scv_server::config::read_layer(&layout.config()).ok();
    let listing = Listing {
        home: layout.home(),
        raw: raw.as_ref(),
        all,
    };
    for kind in scv_channels::ChannelKind::ALL {
        any |= channel(&mut out, &listing, &kind.accounts(layout))?;
    }
    if !any {
        writeln!(out, "  none signed in; see `scv channels login`")?;
    }

    writeln!(
        out,
        "\nAgents ([agents.<name>] in config.toml; homes under {})",
        tilde(&layout.agents())
    )?;
    // Imports compare against the user configuration, which project
    // configuration and flags never change; read it once, when needed.
    let mut user_config = None;
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
        let user_config = user_config.get_or_insert_with(|| {
            Config::load_user(
                layout,
                ConfigOverrides {
                    config_file: overrides.config_file.clone(),
                    ..ConfigOverrides::default()
                },
            )
        });
        let status = match user_config {
            Ok(config) => setup::agent_import_status(config, adapter.name),
            Err(error) => Err(anyhow::anyhow!("{error}")),
        };
        match status {
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

/// What the channel listing needs besides each channel's store.
struct Listing<'a> {
    home: &'a Path,
    /// The instance's `config.toml` as written, when it is readable.
    raw: Option<&'a toml::Value>,
    /// List settings left at their defaults too.
    all: bool,
}

/// The accounts of one channel; whether there were any.
fn channel(
    out: &mut String,
    listing: &Listing<'_>,
    store: &scv_channels::Accounts,
) -> Result<bool> {
    let name = store.kind().name();
    let mut accounts = store.names().unwrap_or_default();
    match store.configured() {
        Ok(configured) => accounts.extend(configured),
        Err(error) => writeln!(out, "  {name}: settings unreadable: {}", safe_error(&error))?,
    }
    accounts.sort();
    accounts.dedup();
    for account in &accounts {
        let (credentials, settings) = store.inspect(account);
        let media = settings.as_ref().ok().and_then(|settings| {
            let table = listing
                .raw
                .and_then(|raw| raw.get("channels")?.get(name)?.get(account)?.get("media"));
            let set = |key: &str| table.is_some_and(|table| table.get(key).is_some());
            let line = media_line(&settings.media, &set);
            (listing.all || line.contains("[config.toml]")).then_some(line)
        });
        let settings = match settings {
            Ok(settings) => {
                let workspace = settings
                    .workspace
                    .as_deref()
                    .map_or_else(|| "the daemon's workspace".to_owned(), tilde);
                let tools = match settings.remote_tools {
                    scv_protocol::RemoteTools::None => "no remote tools",
                    scv_protocol::RemoteTools::Owner => "owner has remote tools",
                };
                let owner_known = credentials
                    .as_ref()
                    .ok()
                    .and_then(Option::as_ref)
                    .map(|credentials| credentials.owner().is_some_and(|owner| !owner.is_empty()));
                let senders = answers(settings.senders, owner_known);
                let enabled = if settings.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                format!("{enabled}, {senders}, {tools}, workspace {workspace}")
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
        let shown = path.strip_prefix(listing.home).unwrap_or(&path);
        writeln!(
            out,
            "  {:<32} credentials {} {credentials}",
            "",
            shown.display()
        )?;
        if let Some(media) = media {
            writeln!(out, "  {:<32} {media}", "")?;
        }
    }
    Ok(!accounts.is_empty())
}

/// Whose messages an account answers, given whether its sign-in records an
/// owner (`None` when it is not signed in or unreadable).
fn answers(senders: scv_protocol::Senders, owner_known: Option<bool>) -> &'static str {
    match (senders, owner_known) {
        (scv_protocol::Senders::Anyone, _) => "answers anyone",
        (scv_protocol::Senders::Owner, Some(false)) => {
            "answers nobody (only its owner, and no owner is recorded)"
        }
        (scv_protocol::Senders::Owner, _) => "answers only its owner",
    }
}

/// An account's media limits (`[channels.<channel>.<account>.media]`), each
/// with where it was set: `config.toml` when `set` says the file names it.
fn media_line(media: &scv_channels::media::MediaSettings, set: &dyn Fn(&str) -> bool) -> String {
    let limits = [
        ("owner_max_mib", media.owner_max_mib),
        ("others_image_max_mib", media.others_image_max_mib),
        ("keep_days", media.keep_days),
    ];
    let limits: Vec<String> = limits
        .iter()
        .map(|(key, value)| {
            let origin = if set(key) { "config.toml" } else { "default" };
            format!("{key} = {value} [{origin}]")
        })
        .collect();
    format!("media {}", limits.join(", "))
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
mod tests;
