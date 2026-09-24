//! Reading and layering configuration: defaults, the user's `config.toml`,
//! a project's `.scv/config.toml`, `SCV_CONFIG`, then environment and flags.

use std::{collections::BTreeMap, io::Write, path::PathBuf};

use anyhow::{Context, Result, bail};
use scv_client::Layout;

use super::{
    Config, ConfigOverrides, ProviderConfig,
    validate::{validate_project_keys, validate_project_not_weaker},
};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

impl Config {
    pub fn init_user_config() -> Result<PathBuf> {
        let path = user_config_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine user config path"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create config directory")?;
            ensure_private_dir(parent)?;
        }
        let content = "[provider]\nactive = \"openai\"\n\n[providers.openai]\nkind = \"openai-compatible\"\nmodel = \"gpt-4.1-mini\"\nbase_url = \"https://api.openai.com/v1\"\napi_key_env = \"OPENAI_API_KEY\"\n";
        if !path.exists() {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("configuration path has no parent"))?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)
                .context("create temporary example configuration")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temporary
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600))
                    .context("secure temporary configuration")?;
            }
            temporary
                .write_all(content.as_bytes())
                .context("write example configuration")?;
            temporary
                .as_file()
                .sync_all()
                .context("sync example configuration")?;
            match temporary.persist(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.error).context("install example configuration"),
            }
        }
        Ok(path)
    }
    pub fn active_provider(&self) -> Result<ProviderConfig> {
        if let Some(name) = self
            .provider_active
            .as_deref()
            .or(self.provider.active.as_deref())
        {
            return self
                .providers
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("active provider profile {name:?} was not found"));
        }
        Ok(self.provider.clone())
    }
}

impl Config {
    pub fn load(workspace: &std::path::Path, overrides: ConfigOverrides) -> Result<Self> {
        Self::load_layers(Some(workspace), overrides)
    }

    /// Load without a project layer, for settings that project configuration
    /// can never set (such as `[agents]`), so the caller's directory is irrelevant.
    pub fn load_user(overrides: ConfigOverrides) -> Result<Self> {
        Self::load_layers(None, overrides)
    }

    fn load_layers(
        workspace: Option<&std::path::Path>,
        overrides: ConfigOverrides,
    ) -> Result<Self> {
        let instance_home = user_home_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine SCV instance home"))?;
        std::fs::create_dir_all(&instance_home).context("create SCV instance home")?;
        ensure_private_dir(&instance_home)?;
        let mut value: toml::Value = toml::from_str(
            &toml::to_string(&Self::default()).context("serialize default configuration")?,
        )?;

        if let Some(user_path) = user_config_path()
            && user_path.is_file()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(&user_path)?.permissions().mode() & 0o077 != 0 {
                    bail!("user configuration is readable by group or others; run chmod 600");
                }
            }
            merge(&mut value, read_layer(&user_path)?);
        }
        let user_baseline: Self = value
            .clone()
            .try_into()
            .context("parse user configuration")?;

        if let Some(workspace) = workspace {
            let project_path = workspace.join(".scv/config.toml");
            // A workspace whose `.scv` is the SCV home (such as running from
            // `~`) has no project layer: that file is the user configuration,
            // already applied above at full trust.
            let user_file = user_config_path().and_then(|path| std::fs::canonicalize(path).ok());
            if project_path.is_file() {
                let canonical_project = std::fs::canonicalize(&project_path)
                    .with_context(|| format!("resolve configuration {}", project_path.display()))?;
                if user_file.as_ref() != Some(&canonical_project) {
                    if !canonical_project.starts_with(workspace) {
                        bail!("project configuration escaped workspace");
                    }
                    let project = read_layer(&canonical_project)?;
                    validate_project_keys(&project)?;
                    let mut candidate_value = value.clone();
                    merge(&mut candidate_value, project);
                    let candidate: Self = candidate_value
                        .clone()
                        .try_into()
                        .context("parse project configuration")?;
                    validate_project_not_weaker(&user_baseline, &candidate)?;
                    value = candidate_value;
                }
            }
        }

        if let Some(explicit) = std::env::var_os("SCV_CONFIG") {
            let path = PathBuf::from(explicit);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(&path)?.permissions().mode() & 0o077 != 0 {
                    bail!("explicit configuration is readable by group or others; run chmod 600");
                }
            }
            let explicit = read_layer(&path)?;
            if explicit.get("channels").is_some() {
                bail!(
                    "{} cannot set [channels]; channel accounts belong in the instance's config.toml",
                    path.display()
                );
            }
            merge(&mut value, explicit);
        }
        let mut config: Self = value.try_into().context("parse merged configuration")?;
        if let Some(name) = overrides.provider.as_deref() {
            config.provider_active = Some(name.to_owned());
        }
        let selected = config.active_provider()?;
        config.provider = selected;
        if let Ok(model) = std::env::var("SCV_MODEL") {
            config.provider.model = model;
        }
        if let Ok(base_url) = std::env::var("SCV_BASE_URL") {
            config.provider.base_url = base_url;
        }
        if let Ok(api_key_env) = std::env::var("SCV_API_KEY_ENV") {
            config.provider.api_key_env = Some(api_key_env);
        }
        if let Some(model) = overrides.model {
            config.provider.model = model;
        }
        if let Some(base_url) = overrides.base_url {
            config.provider.base_url = base_url;
        }
        if let Some(policy) = overrides.approval_policy {
            config.tools.approval_policy = policy;
        }
        if config.skills.user_dir == std::path::Path::new("~/.scv/skills")
            && let Some(home) = std::env::var_os("SCV_HOME")
        {
            config.skills.user_dir = PathBuf::from(home).join("skills");
        }
        config.skills.user_dir = expand_home(&config.skills.user_dir);
        config.instance_home = instance_home;
        config.validate()?;
        Ok(config)
    }

    /// Every leaf setting after layering, as a session started in
    /// `workspace` would see it, with the layer that set it. Secret values are
    /// replaced by `<hidden>`. Channel accounts are left out: `scv config
    /// show` reports them with their credentials.
    pub fn settings_with_origins(
        workspace: Option<&std::path::Path>,
        overrides: &ConfigOverrides,
    ) -> Result<Vec<Setting>> {
        let mut settings: BTreeMap<String, (toml::Value, String)> = BTreeMap::new();
        let mut apply = |value: &toml::Value, origin: &str| {
            flatten(value, String::new(), &mut |key, value| {
                settings.insert(key, (value.clone(), origin.to_owned()));
            });
        };
        let defaults: toml::Value = toml::from_str(
            &toml::to_string(&Self::default()).context("serialize default configuration")?,
        )?;
        apply(&defaults, "default");
        let mut merged = defaults;
        let user = user_config_path().filter(|path| path.is_file());
        if let Some(path) = &user {
            let layer = read_layer(path)?;
            apply(&layer, "config.toml");
            merge(&mut merged, layer);
        }
        if let Some(workspace) = workspace {
            let project = workspace.join(".scv/config.toml");
            let user_file = user
                .as_ref()
                .and_then(|path| std::fs::canonicalize(path).ok());
            if project.is_file() && std::fs::canonicalize(&project).ok() != user_file {
                let layer = read_layer(&project)?;
                apply(&layer, "project .scv/config.toml");
                merge(&mut merged, layer);
            }
        }
        if let Some(path) = std::env::var_os("SCV_CONFIG") {
            let layer = read_layer(std::path::Path::new(&path))?;
            apply(&layer, "SCV_CONFIG");
            merge(&mut merged, layer);
        }
        // Environment and flags change the provider in effect: a named
        // profile's fields when profiles are used, or `[provider]` itself.
        let active = overrides.provider.clone().or_else(|| {
            merged
                .get("provider")?
                .get("active")?
                .as_str()
                .map(ToOwned::to_owned)
        });
        let has_profiles = merged
            .get("providers")
            .and_then(toml::Value::as_table)
            .is_some_and(|profiles| !profiles.is_empty());
        let prefix = match active {
            Some(name) if has_profiles => format!("providers.{name}"),
            _ => "provider".into(),
        };
        let mut set = |key: String, value: String, origin: &str| {
            settings.insert(key, (toml::Value::String(value), origin.to_owned()));
        };
        if let Some(name) = &overrides.provider {
            set("provider.active".into(), name.clone(), "--provider flag");
        }
        for (field, variable) in [
            ("model", "SCV_MODEL"),
            ("base_url", "SCV_BASE_URL"),
            ("api_key_env", "SCV_API_KEY_ENV"),
        ] {
            if let Ok(value) = std::env::var(variable) {
                set(
                    format!("{prefix}.{field}"),
                    value,
                    &format!("env {variable}"),
                );
            }
        }
        for (field, value, flag) in [
            ("model", &overrides.model, "--model flag"),
            ("base_url", &overrides.base_url, "--base-url flag"),
        ] {
            if let Some(value) = value {
                set(format!("{prefix}.{field}"), value.clone(), flag);
            }
        }
        if let Some(policy) = overrides.approval_policy {
            let value = toml::Value::try_from(policy).context("serialize approval policy")?;
            settings.insert(
                "tools.approval_policy".into(),
                (value, "--approval-policy flag".into()),
            );
        }
        Ok(settings
            .into_iter()
            .filter(|(key, _)| !key.starts_with("channels."))
            .map(|(key, (value, origin))| Setting {
                value: if is_secret_key(&key) {
                    "<hidden>".into()
                } else {
                    value.to_string()
                },
                key,
                origin,
            })
            .collect())
    }
}

fn user_config_path() -> Option<PathBuf> {
    user_home_path().map(|path| Layout::new(path).config())
}

pub fn user_home_path() -> Option<PathBuf> {
    let path = Layout::from_env().ok()?.home().to_owned();
    if path.exists() {
        Some(std::fs::canonicalize(path.clone()).unwrap_or(path))
    } else if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

pub(super) fn ensure_private_dir(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure directory {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn read_layer(path: &std::path::Path) -> Result<toml::Value> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("stat configuration {}", path.display()))?
        .len();
    if size > MAX_CONFIG_BYTES {
        bail!("configuration {} exceeds 1 MiB", path.display());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read configuration {}", path.display()))?;
    // The parser's own display quotes the offending line, which may hold a
    // key, so only its message and line number are kept.
    toml::from_str(&content).map_err(|error: toml::de::Error| {
        let line = error.span().map_or_else(String::new, |span| {
            format!(
                " line {}",
                content[..span.start.min(content.len())]
                    .matches('\n')
                    .count()
                    + 1
            )
        });
        anyhow::anyhow!(
            "parse configuration {}{line}: {}",
            path.display(),
            error.message()
        )
    })
}

pub(super) fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// A configuration value in effect and the layer that set it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// Dotted key, such as `tools.approval_policy`.
    pub key: String,
    /// The value as TOML, or `<hidden>` for a secret.
    pub value: String,
    /// `default`, `config.toml`, `project .scv/config.toml`, `SCV_CONFIG`,
    /// `env <VARIABLE>`, or `--<name> flag`.
    pub origin: String,
}

/// Call `visit` with every leaf of `value` under its dotted key.
fn flatten(value: &toml::Value, prefix: String, visit: &mut impl FnMut(String, &toml::Value)) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(value, key, visit);
            }
        }
        leaf => visit(prefix, leaf),
    }
}

/// Keys whose values are credentials: API keys, secrets, passwords, and
/// provider headers, which commonly carry authorization.
pub(super) fn is_secret_key(key: &str) -> bool {
    let last = key.rsplit('.').next().unwrap_or(key);
    last == "api_key"
        || last.ends_with("_api_key")
        || last.contains("secret")
        || last.contains("password")
        || key.split('.').any(|segment| segment == "headers")
}

fn expand_home(path: &std::path::Path) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}
