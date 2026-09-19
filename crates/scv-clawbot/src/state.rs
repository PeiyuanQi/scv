//! Durable ClawBot account, cursor, deduplication, and delivery state.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Account { pub token: String, pub base_url: String }

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PendingDelivery { pub to_user_id: String, pub context_token: String, pub reply: String }

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct BridgeState { pub cursor: String, pub seen: Vec<String>, #[serde(default)] pub pending: Option<PendingDelivery> }

pub fn root() -> Result<PathBuf> {
    std::env::var_os("SCV_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|p| p.join(".scv"))).map(|p| p.join("clawbot")).ok_or_else(|| anyhow!("cannot determine SCV home"))
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') { bail!("invalid ClawBot account name") }
    Ok(())
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("state path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
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
    if !path.exists() { return Ok(None); }
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; if std::fs::metadata(&path)?.permissions().mode() & 0o077 != 0 { bail!("ClawBot account file is accessible by other users") } }
    Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?))
}

pub fn save_account(name: &str, value: &Account) -> Result<()> { validate_name(name)?; atomic_write(&root()?.join("accounts").join(format!("{name}.json")), &serde_json::to_string(value)?) }

pub fn load_state(name: &str) -> Result<BridgeState> {
    validate_name(name)?; let path = root()?.join("state").join(format!("{name}.json"));
    if !path.exists() { return Ok(BridgeState::default()); }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn save_state(name: &str, value: &BridgeState) -> Result<()> { validate_name(name)?; atomic_write(&root()?.join("state").join(format!("{name}.json")), &serde_json::to_string(value)?) }

pub fn remove(name: &str) -> Result<()> {
    validate_name(name)?; let base = root()?;
    for path in [base.join("accounts").join(format!("{name}.json")), base.join("state").join(format!("{name}.json"))] { if path.exists() { std::fs::remove_file(path)?; } }
    Ok(())
}
