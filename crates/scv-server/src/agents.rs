//! Credentials for the native agents SCV delegates to, always inside SCV's
//! private adapter homes: importing a user's own Codex setup, and the
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
            "{} is already SCV's Codex adapter home; pass your own Codex home with --from",
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
        .map(|text| text.parse::<toml::Table>())
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

fn write_private(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("import path has no parent"))?;
    // Named temporary files are created with mode 0600.
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("create temporary import file")?;
    temporary
        .write_all(contents.as_bytes())
        .context("write imported file")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| anyhow!("replace {}: {}", path.display(), error.error))?;
    Ok(())
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
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &silent) } != 0 {
            bail!("disable terminal echo: {}", std::io::Error::last_os_error());
        }
        Ok(Self(original))
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: restores the settings read from the same descriptor.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
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
        KeyStore::JsonEntries(_) | KeyStore::Pi { .. } => {
            bail!("this agent signs in with its own login, not a stored key")
        }
    }
}

/// Describe what `store` holds, never printing a secret.
pub fn stored_status(store: KeyStore, home: &Path) -> Result<(bool, Vec<String>)> {
    match store {
        KeyStore::JsonEntries(path) => {
            let path = home.join(path);
            let entries = read_json_object(&path)?
                .map(|object| object.values().filter(|value| !value.is_null()).count())
                .unwrap_or(0);
            Ok(if entries > 0 {
                (true, vec![format!("signed in ({})", display(&path, home))])
            } else {
                (false, vec!["not signed in".into()])
            })
        }
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
                (true, vec![format!("API key stored as {variable}")])
            } else {
                (false, vec!["not signed in".into()])
            })
        }
        KeyStore::Pi { dir } => pi_status(&home.join(dir)),
    }
}

/// Remove the credentials SCV can see in `store`.
pub fn remove_stored(store: KeyStore, home: &Path) -> Result<Vec<String>> {
    match store {
        KeyStore::JsonEntries(path) | KeyStore::DshRefs { path, .. } => {
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

fn pi_status(dir: &Path) -> Result<(bool, Vec<String>)> {
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
    Ok((ready, lines))
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
mod tests {
    use super::*;

    const CONFIG: &str = r#"model_provider = "relay"
model = "gpt-test"
sandbox_mode = "danger-full-access"
approval_policy = "never"

[model_providers.relay]
base_url = "https://relay.invalid"
wire_api = "responses"
requires_openai_auth = true
experimental_bearer_token = "sk-bearer-secret"
"#;

    fn homes() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn copies_config_and_api_key_privately_without_printing_secrets() {
        let (source, destination) = homes();
        std::fs::write(source.path().join("config.toml"), CONFIG).unwrap();
        let auth = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-auth-secret"}"#;
        std::fs::write(source.path().join("auth.json"), auth).unwrap();
        let notes = import_codex(source.path(), destination.path()).unwrap();
        for file in ["config.toml", "auth.json"] {
            let copied = destination.path().join(file);
            assert_eq!(
                std::fs::read_to_string(&copied).unwrap(),
                std::fs::read_to_string(source.path().join(file)).unwrap()
            );
            assert_eq!(mode(&copied), 0o600);
        }
        let notes = notes.join("\n");
        assert!(notes.contains(r#"model_provider "relay", model "gpt-test""#));
        assert!(notes.contains(r#"sandbox_mode "danger-full-access" and approval_policy "never""#));
        assert!(notes.contains("API-key sign-in"));
        assert!(!notes.contains("secret"));
    }

    #[test]
    fn never_copies_a_chatgpt_session() {
        let (source, destination) = homes();
        std::fs::write(source.path().join("config.toml"), "model = \"m\"\n").unwrap();
        std::fs::write(
            source.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"refresh_token":"rt"}}"#,
        )
        .unwrap();
        let notes = import_codex(source.path(), destination.path()).unwrap();
        assert!(destination.path().join("config.toml").is_file());
        assert!(!destination.path().join("auth.json").exists());
        assert!(notes.join("\n").contains("scv agents login codex"));
    }

    #[test]
    fn env_key_providers_are_flagged() {
        let (source, destination) = homes();
        std::fs::write(
            source.path().join("config.toml"),
            "[model_providers.a]\nenv_key = \"OPENAI_API_KEY\"\n\
             [model_providers.b]\nenv_key = \"RELAY_KEY\"\n",
        )
        .unwrap();
        let notes = import_codex(source.path(), destination.path())
            .unwrap()
            .join("\n");
        assert!(notes.contains("$OPENAI_API_KEY, which SCV removes"));
        assert!(notes.contains("$RELAY_KEY; the SCV daemon's environment must provide it"));
    }

    const DSH: KeyStore = KeyStore::DshRefs {
        path: ".dsh/.credentials.yaml",
        variable: "DEEPSEEK_API_KEY",
    };
    const PI: KeyStore = KeyStore::Pi { dir: ".pi/agent" };

    fn endpoint() -> Endpoint {
        Endpoint {
            base_url: "https://relay.invalid/v1/".into(),
            api: WireApi::Responses,
            model: "gpt-test".into(),
        }
    }

    #[test]
    fn dsh_keys_are_stored_privately_in_its_native_file() {
        let home = tempfile::tempdir().unwrap();
        assert!(!stored_status(DSH, home.path()).unwrap().0);
        let notes = store_key(DSH, home.path(), "sk-dsh-secret").unwrap();
        assert!(!notes.join("\n").contains("secret"));
        let path = home.path().join(".dsh/.credentials.yaml");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "version: 1\n\nrefs:\n  DEEPSEEK_API_KEY: \"sk-dsh-secret\"\n"
        );
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&home.path().join(".dsh")), 0o700);
        let (ready, lines) = stored_status(DSH, home.path()).unwrap();
        assert!(ready);
        assert!(!lines.join("\n").contains("secret"));
        for invalid in ["", "two words", "quote\"d", "line\nbreak"] {
            assert!(store_key(DSH, home.path(), invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            remove_stored(DSH, home.path()).unwrap(),
            ["Removed .dsh/.credentials.yaml"]
        );
        assert!(!path.exists());
        assert!(!stored_status(DSH, home.path()).unwrap().0);
    }

    #[test]
    fn json_entry_stores_report_sign_in_without_values() {
        let home = tempfile::tempdir().unwrap();
        let store = KeyStore::JsonEntries(".grok/auth.json");
        assert!(!stored_status(store, home.path()).unwrap().0);
        std::fs::create_dir(home.path().join(".grok")).unwrap();
        std::fs::write(home.path().join(".grok/auth.json"), "{}").unwrap();
        assert!(!stored_status(store, home.path()).unwrap().0);
        std::fs::write(
            home.path().join(".grok/auth.json"),
            r#"{"https://auth.x.ai::id":{"key":"xai-token-secret"}}"#,
        )
        .unwrap();
        let (ready, lines) = stored_status(store, home.path()).unwrap();
        assert!(ready);
        assert!(!lines.join("\n").contains("secret"));
        std::fs::write(home.path().join(".grok/auth.json"), "xai-token-secret").unwrap();
        let error = stored_status(store, home.path()).unwrap_err().to_string();
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn pi_endpoints_merge_into_pi_files_as_the_private_default() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".pi/agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("models.json"),
            r#"{"providers":{"ollama":{"baseUrl":"http://localhost:11434/v1","api":"openai-completions","models":[{"id":"q"}]}}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"theme":"dark"}"#).unwrap();
        let notes = configure_pi_endpoint(&dir, &endpoint(), "sk-pi-secret").unwrap();
        let notes = notes.join("\n");
        assert!(
            notes.contains("openai-responses at relay.invalid"),
            "{notes}"
        );
        assert!(!notes.contains("secret"));

        let read = |file: &str| -> serde_json::Value {
            let path = dir.join(file);
            assert_eq!(mode(&path), 0o600, "{file}");
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let models = read("models.json");
        assert_eq!(
            models["providers"]["scv"],
            serde_json::json!({
                "baseUrl": "https://relay.invalid/v1",
                "api": "openai-responses",
                "models": [{"id": "gpt-test"}],
                "compat": {"sessionAffinityFormat": "openai-nosession"},
            })
        );
        assert_eq!(models["providers"]["ollama"]["models"][0]["id"], "q");
        assert_eq!(
            read("auth.json")["scv"],
            serde_json::json!({"type": "api_key", "key": "sk-pi-secret"})
        );
        let settings = read("settings.json");
        assert_eq!(settings["defaultProvider"], "scv");
        assert_eq!(settings["defaultModel"], "gpt-test");
        assert_eq!(settings["theme"], "dark");

        let (ready, lines) = stored_status(PI, home.path()).unwrap();
        let lines = lines.join("\n");
        assert!(ready);
        assert!(
            lines.contains(
                r#"default provider "scv" (openai-responses at relay.invalid), model "gpt-test""#
            ),
            "{lines}"
        );
        assert!(!lines.contains("secret"));

        let removed = remove_stored(PI, home.path()).unwrap().join("\n");
        assert!(removed.contains("auth.json"), "{removed}");
        assert!(!dir.join("auth.json").exists());
        let models = read("models.json");
        assert!(models["providers"].get("scv").is_none());
        assert!(models["providers"].get("ollama").is_some());
        let settings = read("settings.json");
        assert!(settings.get("defaultProvider").is_none());
        assert_eq!(settings["theme"], "dark");
        assert!(!stored_status(PI, home.path()).unwrap().0);
    }

    #[test]
    fn pi_endpoint_input_is_validated_before_any_write() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".pi/agent");
        for base_url in [
            "ftp://relay.invalid",
            "https://user:pass@relay.invalid",
            "https://relay.invalid/v1?key=x",
            "https://",
        ] {
            let endpoint = Endpoint {
                base_url: base_url.into(),
                ..endpoint()
            };
            assert!(
                configure_pi_endpoint(&dir, &endpoint, "sk").is_err(),
                "{base_url}"
            );
        }
        let endpoint = Endpoint {
            model: "--flag".into(),
            ..endpoint()
        };
        assert!(configure_pi_endpoint(&dir, &endpoint, "sk").is_err());
        assert!(configure_pi_endpoint(&dir, &super::tests::endpoint(), "").is_err());
        assert!(!dir.exists());

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.json"), "not json").unwrap();
        assert!(configure_pi_endpoint(&dir, &super::tests::endpoint(), "sk").is_err());
        assert!(!dir.join("auth.json").exists());
        assert!(!dir.join("settings.json").exists());
    }

    #[test]
    fn invalid_input_writes_nothing() {
        let (source, destination) = homes();
        std::fs::write(source.path().join("config.toml"), "model = \"m\"\n").unwrap();
        std::fs::write(source.path().join("auth.json"), "not json").unwrap();
        assert!(import_codex(source.path(), destination.path()).is_err());
        std::fs::write(source.path().join("config.toml"), "model = ").unwrap();
        std::fs::remove_file(source.path().join("auth.json")).unwrap();
        assert!(import_codex(source.path(), destination.path()).is_err());
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);

        let empty = tempfile::tempdir().unwrap();
        assert!(import_codex(empty.path(), destination.path()).is_err());
        assert!(import_codex(destination.path(), destination.path()).is_err());
    }
}
