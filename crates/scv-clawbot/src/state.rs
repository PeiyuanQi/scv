//! Durable ClawBot account, cursor, deduplication, and delivery state.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Account { pub token: String, pub base_url: String }

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PendingDelivery {
    #[serde(default)]
    pub message_id: String,
    pub to_user_id: String,
    pub context_token: String,
    pub reply: String,
    /// Stable client IDs make retries of each chunk idempotent.
    #[serde(default)]
    pub client_ids: Vec<String>,
    #[serde(default)]
    pub next_chunk: usize,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct BridgeState { pub cursor: String, pub seen: Vec<String>, #[serde(default)] pub pending: Option<PendingDelivery> }

pub fn root() -> Result<PathBuf> {
    std::env::var_os("SCV_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|p| p.join(".scv"))).map(|p| p.join("clawbot")).ok_or_else(|| anyhow!("cannot determine SCV home"))
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') { bail!("invalid ClawBot account name") }
    Ok(())
}

#[derive(Deserialize)]
struct LegacyAccount {
    token: String,
    base_url: String,
}

fn check_private(path: &Path, kind: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            bail!("ClawBot {kind} file is accessible by other users")
        }
    }
    Ok(())
}

fn read_account_file(path: &Path) -> Result<Account> {
    check_private(path, "account")?;
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

fn migrate_legacy_default() -> Result<Option<Account>> {
    let legacy = root()?.parent().ok_or_else(|| anyhow!("ClawBot state path has no parent"))?.join("clawbot.toml");
    if !legacy.exists() {
        return Ok(None);
    }
    check_private(&legacy, "legacy credential")?;
    let value: LegacyAccount = toml::from_str(&std::fs::read_to_string(&legacy)?)
        .map_err(|error| anyhow!("parse legacy ClawBot credentials: {error}"))?;
    let account = Account { token: value.token, base_url: value.base_url };
    save_account("default", &account)?;
    std::fs::remove_file(&legacy)?;
    Ok(Some(account))
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("state path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        if matches!(parent.file_name().and_then(|name| name.to_str()), Some("accounts" | "state"))
            && let Some(root) = parent.parent()
            && root.file_name().and_then(|name| name.to_str()) == Some("clawbot")
        {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temp.write_all(contents.as_bytes())?; temp.as_file().sync_all()?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o600))?; }
    temp.persist(path).map_err(|e| anyhow!("atomic state replace failed: {}", e.error))?;
    Ok(())
}

pub fn account(name: &str) -> Result<Option<Account>> {
    validate_name(name)?; let path = root()?.join("accounts").join(format!("{name}.json"));
    if path.exists() { return Ok(Some(read_account_file(&path)?)); }
    if name == "default" { return migrate_legacy_default(); }
    Ok(None)
}

pub fn save_account(name: &str, value: &Account) -> Result<()> { validate_name(name)?; atomic_write(&root()?.join("accounts").join(format!("{name}.json")), &serde_json::to_string(value)?) }

pub fn load_state(name: &str) -> Result<BridgeState> {
    validate_name(name)?; let path = root()?.join("state").join(format!("{name}.json"));
    if !path.exists() { return Ok(BridgeState::default()); }
    check_private(&path, "state")?;
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn save_state(name: &str, value: &BridgeState) -> Result<()> { validate_name(name)?; atomic_write(&root()?.join("state").join(format!("{name}.json")), &serde_json::to_string(value)?) }

pub fn remove(name: &str) -> Result<()> {
    validate_name(name)?; let base = root()?;
    for path in [base.join("accounts").join(format!("{name}.json")), base.join("state").join(format!("{name}.json"))] { if path.exists() { std::fs::remove_file(path)?; } }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_state_reads_legacy_shape() {
        let state: BridgeState = serde_json::from_str(
            r#"{"cursor":"c","seen":["m"],"pending":{"to_user_id":"u","context_token":"x","reply":"hello"}}"#,
        )
        .unwrap();
        let pending = state.pending.unwrap();
        assert!(pending.client_ids.is_empty());
        assert_eq!(pending.next_chunk, 0);
    }
}
