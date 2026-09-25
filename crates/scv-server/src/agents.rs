//! Credentials for the native agents SCV delegates to, always inside SCV's
//! private agent homes: importing a user's own Codex or Grok setup, and the
//! API-key and endpoint stores SCV writes in an agent CLI's native format.

use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use scv_tools::adapters::KeyStore;

const MAX_IMPORT_BYTES: u64 = 1024 * 1024;

enum Auth {
    /// An API key, which is static and safe to hold in two homes.
    ApiKey,
    /// A ChatGPT sign-in, whose rotating refresh token must stay in one home.
    Session,
    None,
}

/// Copy `config.toml` and an API-key `auth.json` from a Codex home into
/// `destination`, returning display lines that never contain secret values.
/// Both files are validated before either is written.
pub fn import_codex(source: &Path, destination: &Path) -> Result<Vec<String>> {
    let source = std::fs::canonicalize(source)
        .with_context(|| format!("resolve Codex home {}", source.display()))?;
    let destination = std::fs::canonicalize(destination)
        .with_context(|| format!("resolve SCV Codex home {}", destination.display()))?;
    if source == destination {
        bail!(
            "{} is already SCV's Codex agent home; pass your own Codex home with --from",
            source.display()
        );
    }
    let config = read_bounded(&source.join("config.toml"))?;
    let auth = read_bounded(&source.join("auth.json"))?;
    if config.is_none() && auth.is_none() {
        bail!("no config.toml or auth.json in {}", source.display());
    }
    let table = config
        .as_deref()
        .map(str::parse::<toml::Table>)
        .transpose()
        .context("parse Codex config.toml")?;
    let auth_kind = auth.as_deref().map(classify_auth).transpose()?;

    let mut notes = Vec::new();
    if let (Some(text), Some(table)) = (&config, &table) {
        write_private(&destination.join("config.toml"), text)?;
        notes.push(format!("Copied config.toml{}", describe(table)));
        notes.extend(config_notes(table));
    }
    match (&auth, auth_kind) {
        (Some(text), Some(Auth::ApiKey)) => {
            write_private(&destination.join("auth.json"), text)?;
            notes.push("Copied the API-key sign-in from auth.json".into());
        }
        (Some(_), Some(Auth::Session)) => notes.push(
            "Skipped auth.json: a ChatGPT sign-in's refresh token must not be shared; \
             sign SCV in separately with `scv agents login codex`"
                .into(),
        ),
        (Some(_), Some(Auth::None)) => notes.push("Skipped auth.json: it holds no API key".into()),
        _ => {}
    }
    Ok(notes)
}

/// The files [`import_codex`] copies from `source`: `config.toml` when
/// present, and `auth.json` when it holds an API key.
pub fn codex_copied_files(source: &Path) -> Vec<String> {
    let mut files = Vec::new();
    if source.join("config.toml").is_file() {
        files.push("config.toml".to_owned());
    }
    if let Ok(Some(text)) = read_bounded(&source.join("auth.json"))
        && matches!(classify_auth(&text), Ok(Auth::ApiKey))
    {
        files.push("auth.json".to_owned());
    }
    files
}

fn read_bounded(path: &Path) -> Result<Option<String>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(anyhow!(error).context(format!("open {}", path.display()))),
    };
    if !file.metadata()?.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mut text = String::new();
    file.take(MAX_IMPORT_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("read {}", path.display()))?;
    if text.len() as u64 > MAX_IMPORT_BYTES {
        bail!("{} exceeds 1 MiB", path.display());
    }
    Ok(Some(text))
}

fn classify_auth(text: &str) -> Result<Auth> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| anyhow!("Codex auth.json is not valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("Codex auth.json is not a JSON object"))?;
    if object.get("tokens").is_some_and(|tokens| !tokens.is_null()) {
        return Ok(Auth::Session);
    }
    let has_key = object
        .get("OPENAI_API_KEY")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|key| !key.trim().is_empty());
    Ok(if has_key { Auth::ApiKey } else { Auth::None })
}

/// Non-secret identifying settings, debug-quoted so they are terminal-safe.
fn describe(table: &toml::Table) -> String {
    let fields: Vec<String> = ["model_provider", "model"]
        .into_iter()
        .filter_map(|key| Some(format!("{key} {:?}", table.get(key)?.as_str()?)))
        .collect();
    if fields.is_empty() {
        String::new()
    } else {
        format!(" ({})", fields.join(", "))
    }
}

fn config_notes(table: &toml::Table) -> Vec<String> {
    let mut notes = Vec::new();
    let policy: Vec<String> = ["sandbox_mode", "approval_policy"]
        .into_iter()
        .filter_map(|key| Some(format!("{key} {:?}", table.get(key)?.as_str()?)))
        .collect();
    if !policy.is_empty() {
        notes.push(format!(
            "Delegated Codex runs also use {}",
            policy.join(" and ")
        ));
    }
    let providers = table
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten();
    for (name, provider) in providers {
        let Some(variable) = provider.get("env_key").and_then(toml::Value::as_str) else {
            continue;
        };
        notes.push(
            if scv_tools::adapters::is_removed_agent_variable(std::ffi::OsStr::new(variable)) {
                format!(
                    "Warning: provider {name:?} reads its key from ${variable}, which SCV \
                     removes from delegated agents; keep the key in auth.json \
                     (requires_openai_auth) or experimental_bearer_token instead"
                )
            } else {
                format!(
                    "Note: provider {name:?} reads its key from ${variable}; the SCV daemon's \
                     environment must provide it (the user service does not load your shell profile)"
                )
            },
        );
    }
    notes
}

/// Copy the user's Grok `config.toml` from `source` (a Grok home) into
/// `destination` (SCV's Grok home), returning display lines that never
/// contain secret values. Top-level tables from the user's file win; tables
/// only SCV's copy has, such as the `[marketplace]` state Grok writes there,
/// are kept. `auth.json` sign-ins are never copied. The merged file is
/// validated before anything is written.
pub fn import_grok(source: &Path, destination: &Path) -> Result<Vec<String>> {
    let source = std::fs::canonicalize(source)
        .with_context(|| format!("resolve Grok home {}", source.display()))?;
    std::fs::create_dir_all(destination)
        .with_context(|| format!("create {}", destination.display()))?;
    let destination = std::fs::canonicalize(destination)
        .with_context(|| format!("resolve SCV Grok home {}", destination.display()))?;
    if source == destination {
        bail!(
            "{} is already SCV's Grok home; pass your own Grok home with --from",
            source.display()
        );
    }
    let text = read_bounded(&source.join("config.toml"))?
        .ok_or_else(|| anyhow!("no config.toml in {}", source.display()))?;
    // Never echo parse errors' source text: these files hold keys.
    let user: toml::Table = text
        .parse()
        .map_err(|_| anyhow!("your Grok config.toml is not valid TOML"))?;
    let target = destination.join("config.toml");
    let existing: toml::Table = match read_bounded(&target)? {
        Some(existing) => existing.parse().map_err(|_| {
            anyhow!(
                "SCV's Grok config.toml ({}) is not valid TOML; move it aside and import again",
                target.display()
            )
        })?,
        None => toml::Table::new(),
    };
    let kept: toml::Table = existing
        .into_iter()
        .filter(|(key, _)| !user.contains_key(key))
        .collect();
    let merged = if kept.values().all(toml::Value::is_table) {
        // Appending whole tables keeps the user's own formatting and comments.
        let mut merged = text.trim_end().to_owned();
        merged.push('\n');
        if !kept.is_empty() {
            merged.push('\n');
            merged.push_str(&toml::to_string(&kept).context("serialize kept settings")?);
        }
        merged
    } else {
        // A kept top-level value would land inside the user's last table if
        // appended, so write the merged table instead.
        let mut table = user.clone();
        table.extend(kept.clone());
        toml::to_string(&table).context("serialize merged Grok config")?
    };
    if merged.parse::<toml::Table>().is_err() {
        bail!("the merged Grok config.toml would not be valid TOML; nothing was written");
    }
    write_private(&target, &merged)?;

    let mut notes = vec![format!("Copied config.toml{}", describe_grok(&user))];
    if !kept.is_empty() {
        let names: Vec<String> = kept.keys().map(|key| format!("{key:?}")).collect();
        notes.push(format!("Kept SCV-only settings: {}", names.join(", ")));
    }
    match grok_default_key(&user) {
        GrokKey::InConfig(_) | GrokKey::NoDefault => {}
        GrokKey::FromVariable(model, variable) => notes.push(grok_variable_note(&model, &variable)),
        GrokKey::Missing(model) => notes.push(format!(
            "Note: default model {model:?} has no api_key in config; sign SCV in with \
             `scv agents login grok` or add api_key to its profile"
        )),
    }
    if source.join("auth.json").exists() {
        notes.push(
            "Skipped auth.json: `grok login` sign-ins are not shared; sign SCV in \
             separately with `scv agents login grok` if you need one"
                .into(),
        );
    }
    Ok(notes)
}

/// Profiles and the default model, debug-quoted so they are terminal-safe.
fn describe_grok(table: &toml::Table) -> String {
    let profiles: Vec<String> = table
        .get("model")
        .and_then(toml::Value::as_table)
        .map(|models| models.keys().map(|key| format!("{key:?}")).collect())
        .unwrap_or_default();
    let default =
        grok_default(table).map_or_else(|| "built-in".into(), |model| format!("{model:?}"));
    if profiles.is_empty() {
        format!(" (default model {default}; no model profiles)")
    } else {
        format!(
            " (default model {default}; profiles {})",
            profiles.join(", ")
        )
    }
}

/// Where the key for Grok's default model comes from.
enum GrokKey {
    /// No `[models] default`: Grok's built-in default needs `grok login`.
    NoDefault,
    /// The default's profile holds an `api_key`.
    InConfig(String),
    /// The default's profile reads its key from these variables.
    FromVariable(String, Vec<String>),
    /// The default has no profile key.
    Missing(String),
}

fn grok_default(table: &toml::Table) -> Option<String> {
    table
        .get("models")?
        .get("default")?
        .as_str()
        .map(ToOwned::to_owned)
}

/// Resolve the default model's profile, by catalog key or by model id as
/// Grok does, and say where its key comes from.
fn grok_default_key(table: &toml::Table) -> GrokKey {
    let Some(default) = grok_default(table) else {
        return GrokKey::NoDefault;
    };
    let models = table.get("model").and_then(toml::Value::as_table);
    let profile = models.and_then(|models| {
        models.get(&default).or_else(|| {
            models.values().find(|profile| {
                profile.get("model").and_then(toml::Value::as_str) == Some(&default)
            })
        })
    });
    let Some(profile) = profile else {
        return GrokKey::Missing(default);
    };
    if profile
        .get("api_key")
        .and_then(toml::Value::as_str)
        .is_some_and(|key| !key.trim().is_empty())
    {
        return GrokKey::InConfig(default);
    }
    let variables: Vec<String> = match profile.get("env_key") {
        Some(toml::Value::String(name)) => vec![name.clone()],
        Some(toml::Value::Array(names)) => names
            .iter()
            .filter_map(toml::Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    if variables.is_empty() {
        GrokKey::Missing(default)
    } else {
        GrokKey::FromVariable(default, variables)
    }
}

/// A variable Grok can read in a delegated run: set here and not one SCV
/// removes from delegated agents.
fn usable_grok_variable(variables: &[String]) -> Option<&String> {
    variables.iter().find(|variable| {
        !scv_tools::adapters::is_removed_agent_variable(std::ffi::OsStr::new(variable.as_str()))
            && std::env::var_os(variable).is_some_and(|value| !value.is_empty())
    })
}

fn grok_variable_note(model: &str, variables: &[String]) -> String {
    let names: Vec<String> = variables.iter().map(|name| format!("${name}")).collect();
    format!(
        "Note: default model {model:?} reads its key from {}; SCV removes key variables \
         from delegated agents and its service does not load your shell profile, so put \
         api_key in the profile instead",
        names.join(" or ")
    )
}

/// Whether an agent's stored sign-in is usable, with display lines that
/// never contain a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredStatus {
    pub ready: bool,
    pub lines: Vec<String>,
}

impl StoredStatus {
    fn ready(line: String) -> Self {
        Self {
            ready: true,
            lines: vec![line],
        }
    }

    fn missing(lines: Vec<String>) -> Self {
        Self {
            ready: false,
            lines,
        }
    }
}

fn grok_status(auth: &Path, config: &Path, home: &Path) -> Result<StoredStatus> {
    let entries = read_json_object(auth)?.map_or(0, |object| {
        object.values().filter(|value| !value.is_null()).count()
    });
    if entries > 0 {
        return Ok(StoredStatus::ready(format!(
            "signed in ({})",
            display(auth, home)
        )));
    }
    let Some(text) = read_bounded(config)? else {
        return Ok(StoredStatus::missing(vec!["not signed in".into()]));
    };
    let table: toml::Table = text
        .parse()
        .map_err(|_| anyhow!("{} is not valid TOML", display(config, home)))?;
    Ok(match grok_default_key(&table) {
        GrokKey::InConfig(model) => {
            StoredStatus::ready(format!("signed in (API key in config, model {model:?})"))
        }
        GrokKey::FromVariable(model, variables) => match usable_grok_variable(&variables) {
            Some(variable) => {
                StoredStatus::ready(format!("signed in (key from ${variable}, model {model:?})"))
            }
            None => StoredStatus::missing(vec![
                "not signed in".into(),
                grok_variable_note(&model, &variables),
            ]),
        },
        GrokKey::Missing(model) => StoredStatus::missing(vec![
            "not signed in".into(),
            format!("default model {model:?} has no api_key in config"),
        ]),
        GrokKey::NoDefault => StoredStatus::missing(vec!["not signed in".into()]),
    })
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    scv_client::fs::replace_private(path, contents.as_bytes())
        .with_context(|| format!("replace {}", path.display()))
}

/// Longest API key or endpoint field SCV accepts.
const MAX_FIELD_BYTES: usize = 4096;

/// The pi provider id SCV writes for an OpenAI-compatible endpoint.
pub const PI_PROVIDER: &str = "scv";

/// Which OpenAI wire protocol an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireApi {
    Responses,
    ChatCompletions,
}

impl WireApi {
    fn pi_api(self) -> &'static str {
        match self {
            Self::Responses => "openai-responses",
            Self::ChatCompletions => "openai-completions",
        }
    }
}

/// An OpenAI-compatible endpoint for pi, without its key.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub api: WireApi,
    pub model: String,
}

/// Read a secret from the terminal without echo, or from piped stdin.
pub fn read_secret(prompt: &str) -> Result<String> {
    use std::io::{BufRead as _, IsTerminal as _};
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.is_terminal() {
        eprint!("{prompt}: ");
        std::io::stderr().flush().ok();
        let _echo = EchoOff::new()?;
        stdin.lock().read_line(&mut line)?;
        eprintln!();
    } else {
        stdin
            .lock()
            .take(MAX_FIELD_BYTES as u64 + 2)
            .read_line(&mut line)?;
    }
    let secret = line.trim().to_owned();
    validate_secret(&secret)?;
    Ok(secret)
}

/// Terminal echo disabled for the guard's lifetime.
struct EchoOff(libc::termios);

impl EchoOff {
    fn new() -> Result<Self> {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the termios struct for a valid descriptor.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) } != 0 {
            bail!(
                "read terminal settings: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: tcgetattr succeeded, so the struct is initialized.
        let original = unsafe { termios.assume_init() };
        let mut silent = original;
        silent.c_lflag &= !libc::ECHO;
        // SAFETY: a valid descriptor and a termios derived from its own settings.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const silent) } != 0 {
            bail!("disable terminal echo: {}", std::io::Error::last_os_error());
        }
        Ok(Self(original))
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restores the settings read from the same descriptor.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.0) };
    }
}

fn validate_secret(secret: &str) -> Result<()> {
    if secret.is_empty() {
        bail!("no key entered");
    }
    if secret.len() > MAX_FIELD_BYTES
        || secret
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\')
    {
        bail!("the key must be one line of at most {MAX_FIELD_BYTES} printable characters");
    }
    Ok(())
}

/// Store an API key in `store`, inside the adapter `home`.
pub fn store_key(store: KeyStore, home: &Path, key: &str) -> Result<Vec<String>> {
    validate_secret(key)?;
    match store {
        KeyStore::DshRefs { path, variable } => {
            let path = home.join(path);
            create_private_dirs(home, &path)?;
            // serde_json quoting is a valid YAML double-quoted scalar.
            let quoted = serde_json::to_string(key)?;
            write_private(
                &path,
                &format!("version: 1\n\nrefs:\n  {variable}: {quoted}\n"),
            )?;
            Ok(vec![format!(
                "Stored the API key as {variable} in {}",
                display(&path, home)
            )])
        }
        KeyStore::Grok { .. } | KeyStore::Pi { .. } => {
            bail!("this agent signs in with its own login, not a stored key")
        }
        KeyStore::Scv { .. } => {
            bail!("the nested SCV uses SCV's own provider: run `scv agents import scv`")
        }
    }
}

/// Describe what `store` holds, never printing a secret.
pub fn stored_status(store: KeyStore, home: &Path) -> Result<StoredStatus> {
    match store {
        KeyStore::Grok { auth, config } => grok_status(&home.join(auth), &home.join(config), home),
        KeyStore::DshRefs { path, variable } => {
            let path = home.join(path);
            let stored = read_bounded(&path)?.is_some_and(|text| {
                text.lines().any(|line| {
                    line.trim_start()
                        .strip_prefix(variable)
                        .and_then(|rest| rest.strip_prefix(':'))
                        .is_some_and(|value| !matches!(value.trim(), "" | "\"\"" | "''"))
                })
            });
            Ok(if stored {
                StoredStatus::ready(format!("API key stored as {variable}"))
            } else {
                StoredStatus::missing(vec!["not signed in".into()])
            })
        }
        KeyStore::Pi { dir } => pi_status(&home.join(dir)),
        KeyStore::Scv { config } => scv_child_status(&home.join(config)),
    }
}

/// Remove the credentials SCV can see in `store`.
pub fn remove_stored(store: KeyStore, home: &Path) -> Result<Vec<String>> {
    match store {
        KeyStore::Grok { auth: path, .. }
        | KeyStore::DshRefs { path, .. }
        | KeyStore::Scv { config: path } => {
            let path = home.join(path);
            Ok(vec![if remove_if_present(&path)? {
                format!("Removed {}", display(&path, home))
            } else {
                "Nothing stored".into()
            }])
        }
        KeyStore::Pi { dir } => {
            let dir = home.join(dir);
            let mut notes = Vec::new();
            if remove_if_present(&dir.join("auth.json"))? {
                notes.push("Removed pi's stored sign-ins (auth.json)".into());
            }
            let models = dir.join("models.json");
            if let Some(mut object) = read_json_object(&models)?
                && let Some(providers) = object
                    .get_mut("providers")
                    .and_then(serde_json::Value::as_object_mut)
                && providers.remove(PI_PROVIDER).is_some()
            {
                write_json(&models, &object)?;
                notes.push(format!(
                    "Removed the {PI_PROVIDER} endpoint from models.json"
                ));
            }
            let settings = dir.join("settings.json");
            if let Some(mut object) = read_json_object(&settings)?
                && object
                    .get("defaultProvider")
                    .and_then(serde_json::Value::as_str)
                    == Some(PI_PROVIDER)
            {
                object.remove("defaultProvider");
                object.remove("defaultModel");
                write_json(&settings, &object)?;
                notes.push("Cleared pi's default model".into());
            }
            if notes.is_empty() {
                notes.push("Nothing stored".into());
            }
            Ok(notes)
        }
    }
}

/// The provider profile name `scv agents import scv` writes for the nested SCV.
pub const SCV_CHILD_PROVIDER: &str = "scv";

/// The parts of SCV's own provider that the nested SCV copies.
pub struct ScvChildProvider<'a> {
    pub wire_api: &'a str,
    pub model: &'a str,
    pub base_url: &'a str,
    pub timeout_seconds: u64,
    pub headers: &'a std::collections::HashMap<String, scv_client::Secret>,
    /// Offer the endpoint's hosted web search, as SCV's own config does.
    pub hosted_web_search: bool,
}

/// Write the nested SCV's `config.toml` in `home` (mode 0600): SCV's own
/// provider as profile [`SCV_CHILD_PROVIDER`], with `key` stored in the file
/// because delegated agents never inherit key variables. Other settings
/// already in the file are kept. Returns display lines without the key.
pub fn configure_scv_child(
    home: &Path,
    provider: &ScvChildProvider<'_>,
    key: &str,
) -> Result<Vec<String>> {
    validate_secret(key)?;
    let base_url = validate_base_url(provider.base_url)?;
    if !valid_model_id(provider.model) {
        bail!("invalid model id {:?}", provider.model);
    }
    let path = home.join("config.toml");
    create_private_dirs(home, &path)?;
    let mut table: toml::Table = match read_bounded(&path)? {
        Some(text) => text
            .parse()
            .with_context(|| format!("parse existing {}", display(&path, home)))?,
        None => toml::Table::new(),
    };
    let mut selection = toml::Table::new();
    selection.insert("active".into(), SCV_CHILD_PROVIDER.into());
    table.insert("provider".into(), selection.into());
    let mut profile = toml::Table::new();
    profile.insert("kind".into(), "openai-compatible".into());
    profile.insert("wire_api".into(), provider.wire_api.into());
    profile.insert("model".into(), provider.model.into());
    profile.insert("base_url".into(), base_url.clone().into());
    profile.insert("api_key".into(), key.into());
    profile.insert(
        "timeout_seconds".into(),
        i64::try_from(provider.timeout_seconds)
            .unwrap_or(i64::MAX)
            .into(),
    );
    if !provider.headers.is_empty() {
        let headers: toml::Table = provider
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.expose().into()))
            .collect();
        profile.insert("headers".into(), headers.into());
    }
    let providers = table
        .entry("providers")
        .or_insert_with(|| toml::Table::new().into());
    let Some(providers) = providers.as_table_mut() else {
        bail!(
            "{} has a `providers` value that is not a table",
            display(&path, home)
        );
    };
    providers.insert(SCV_CHILD_PROVIDER.into(), profile.into());
    if provider.hosted_web_search {
        let web = table
            .entry("web")
            .or_insert_with(|| toml::Table::new().into());
        let Some(web) = web.as_table_mut() else {
            bail!(
                "{} has a `web` value that is not a table",
                display(&path, home)
            );
        };
        web.insert("search".into(), "provider".into());
    }
    let text = toml::to_string(&table).context("encode the nested SCV config")?;
    text.parse::<toml::Table>()
        .context("the nested SCV config did not round-trip")?;
    write_private(&path, &text)?;
    let mut lines = vec![
        format!(
            "Wrote {} (mode 0600): provider {SCV_CHILD_PROVIDER:?}, model {:?} at {}",
            display(&path, home),
            provider.model,
            host(&base_url)
        ),
        "The API key is stored in that file and not shown".into(),
    ];
    if provider.hosted_web_search {
        lines.push("Hosted web search is on, as in SCV's own config".into());
    }
    Ok(lines)
}

/// What the nested SCV's `config.toml` provides, never printing its key.
fn scv_child_status(path: &Path) -> Result<StoredStatus> {
    let Some(text) = read_bounded(path)? else {
        return Ok(StoredStatus::missing(vec![
            "not configured: run `scv agents import scv`".into(),
        ]));
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return Ok(StoredStatus::missing(vec![
            "config.toml is not valid TOML".into(),
        ]));
    };
    let active = table
        .get("provider")
        .and_then(|provider| provider.get("active"))
        .and_then(toml::Value::as_str);
    let profile = active.and_then(|active| {
        table
            .get("providers")
            .and_then(|providers| providers.get(active))
            .and_then(toml::Value::as_table)
    });
    let (Some(active), Some(profile)) = (active, profile) else {
        return Ok(StoredStatus::missing(vec![
            "no active provider: run `scv agents import scv`".into(),
        ]));
    };
    let text = |key: &str| profile.get(key).and_then(toml::Value::as_str);
    let keyed = text("api_key").is_some_and(|key| !key.trim().is_empty());
    let location = format!(
        "provider {active:?}: model {:?} at {}",
        text("model").unwrap_or("unset"),
        text("base_url").map_or_else(|| "no base URL".into(), host)
    );
    Ok(if keyed {
        StoredStatus::ready(format!("{location}, API key stored"))
    } else {
        StoredStatus::missing(vec![format!(
            "{location}, no stored API key (a key variable is not inherited): run `scv agents import scv`"
        )])
    })
}

/// Point pi at an OpenAI-compatible endpoint as provider [`PI_PROVIDER`]
/// and make it pi's default, storing the key in pi's `auth.json`.
pub fn configure_pi_endpoint(dir: &Path, endpoint: &Endpoint, key: &str) -> Result<Vec<String>> {
    validate_secret(key)?;
    let base_url = validate_base_url(&endpoint.base_url)?;
    if !valid_model_id(&endpoint.model) {
        bail!("invalid model id {:?}", endpoint.model);
    }
    create_private_dirs(dir, &dir.join("auth.json"))?;
    // Validate every existing file before changing any of them.
    let mut models = read_json_object(&dir.join("models.json"))?.unwrap_or_default();
    let mut auth = read_json_object(&dir.join("auth.json"))?.unwrap_or_default();
    let mut settings = read_json_object(&dir.join("settings.json"))?.unwrap_or_default();

    let providers = models
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}));
    let providers = providers
        .as_object_mut()
        .ok_or_else(|| anyhow!("pi models.json has a non-object \"providers\""))?;
    let mut provider = serde_json::json!({
        "baseUrl": base_url,
        "api": endpoint.api.pi_api(),
        "models": [{"id": endpoint.model}],
    });
    if endpoint.api == WireApi::Responses {
        // pi's default OpenAI affinity header is `session_id`; proxies that
        // reject underscores in header names answer it with HTTP 520
        // (verified against a relay). `x-client-request-id` still goes out.
        provider["compat"] = serde_json::json!({"sessionAffinityFormat": "openai-nosession"});
    }
    providers.insert(PI_PROVIDER.into(), provider);
    auth.insert(
        PI_PROVIDER.into(),
        serde_json::json!({"type": "api_key", "key": key}),
    );
    settings.insert("defaultProvider".into(), PI_PROVIDER.into());
    settings.insert("defaultModel".into(), endpoint.model.clone().into());

    write_json(&dir.join("models.json"), &models)?;
    write_json(&dir.join("auth.json"), &auth)?;
    write_json(&dir.join("settings.json"), &settings)?;
    Ok(vec![
        format!(
            "Configured pi provider {PI_PROVIDER:?}: {} at {}, model {:?}",
            endpoint.api.pi_api(),
            host(&base_url),
            endpoint.model
        ),
        "Stored its API key in pi's auth.json (mode 0600) and made it pi's default".into(),
    ])
}

fn pi_status(dir: &Path) -> Result<StoredStatus> {
    let settings = read_json_object(&dir.join("settings.json"))?.unwrap_or_default();
    let auth = read_json_object(&dir.join("auth.json"))?.unwrap_or_default();
    let models = read_json_object(&dir.join("models.json"))?.unwrap_or_default();
    let mut lines = Vec::new();
    let text = |object: &serde_json::Map<String, serde_json::Value>, key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    };
    let provider = text(&settings, "defaultProvider");
    if let Some(provider) = &provider {
        let endpoint = models
            .get("providers")
            .and_then(|providers| providers.get(provider))
            .and_then(serde_json::Value::as_object);
        let location = endpoint
            .map(|endpoint| {
                format!(
                    " ({} at {})",
                    text(endpoint, "api").unwrap_or_else(|| "api unset".into()),
                    text(endpoint, "baseUrl")
                        .map_or_else(|| "no base URL".into(), |url| host(&url))
                )
            })
            .unwrap_or_default();
        lines.push(format!(
            "default provider {provider:?}{location}, model {:?}",
            text(&settings, "defaultModel").unwrap_or_else(|| "unset".into())
        ));
    }
    let mut signed_in: Vec<&String> = auth.keys().collect();
    signed_in.sort();
    let ready = !signed_in.is_empty();
    if ready {
        let names: Vec<String> = signed_in.iter().map(|name| format!("{name:?}")).collect();
        lines.push(format!("stored sign-ins: {}", names.join(", ")));
    } else {
        lines.push("not signed in".into());
    }
    Ok(StoredStatus { ready, lines })
}

fn validate_base_url(value: &str) -> Result<String> {
    let value = value.trim().trim_end_matches('/');
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .ok_or_else(|| anyhow!("the base URL must start with https:// or http://"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty()
        || authority.contains('@')
        || value.len() > MAX_FIELD_BYTES
        || value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '#'))
    {
        bail!("the base URL must be a plain http(s) URL without credentials, query, or fragment");
    }
    Ok(value.to_owned())
}

/// The host of a validated URL, for display.
fn host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_owned()
}

fn valid_model_id(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && !model.starts_with(['-', '@'])
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@[]-".contains(c))
}

fn display(path: &Path, home: &Path) -> String {
    path.strip_prefix(home).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    )
}

fn read_json_object(path: &Path) -> Result<Option<serde_json::Map<String, serde_json::Value>>> {
    let Some(text) = read_bounded(path)? else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(Some(serde_json::Map::new()));
    }
    match serde_json::from_str(&text) {
        Ok(serde_json::Value::Object(object)) => Ok(Some(object)),
        // Never echo the content: these files hold keys.
        _ => bail!("{} is not a JSON object", path.display()),
    }
}

fn write_json(path: &Path, object: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    write_private(
        path,
        &format!("{}\n", serde_json::to_string_pretty(object)?),
    )
}

fn remove_if_present(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(anyhow!(error).context(format!("remove {}", path.display()))),
    }
}

/// Create the directories between `root` and `file` with mode 0700.
fn create_private_dirs(root: &Path, file: &Path) -> Result<()> {
    let parent = file
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", file.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    use std::os::unix::fs::PermissionsExt as _;
    let mut dir = parent;
    loop {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        match dir.parent() {
            Some(next) if next.starts_with(root) && next != root => dir = next,
            _ => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
