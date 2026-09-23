//! Importing a user's own Codex setup into SCV's private Codex adapter home.

use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

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
            if scv_tools::AGENT_REMOVED_ENVIRONMENT.contains(&variable) {
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
