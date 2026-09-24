//! `chat_attach`: send a file with a chat session's reply.
//!
//! The tool checks the file, copies it into the channels' media outbox, and
//! reports the copy; the chat client that owns the session sends it after
//! the reply text and sends nothing from anywhere else. Because model input
//! can carry injected instructions, the tool refuses anything that is not a
//! regular file, anything over the size limit, and every known secret
//! location, judged after symlinks resolve. This stops a model from mailing
//! out keys by path; a model with a shell can still copy data elsewhere, so
//! it is a guard, not a sandbox.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use scv_protocol::{CHAT_ATTACH_TOOL, ReplyAttachment};
use serde::Deserialize;
use serde_json::{Value, json};

/// Largest caption, in bytes.
const MAX_CAPTION_BYTES: usize = 1024;

/// Paths under the user's home that hold credentials, keys, or browser
/// profiles.
const HOME_SECRETS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/gh",
    ".config/gcloud",
    ".config/hub",
    ".config/google-chrome",
    ".config/chromium",
    ".mozilla",
    ".password-store",
    ".local/share/keyrings",
    ".codex",
    ".claude",
    ".claude.json",
    ".grok",
    ".scv",
];

/// System paths that hold host secrets or are not files.
const SYSTEM_SECRETS: &[&str] = &[
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/ssh",
    "/etc/sudoers",
    "/etc/sudoers.d",
    "/root",
    "/proc",
    "/sys",
    "/dev",
];

/// Where `chat_attach` may read from, and where its copies go.
#[derive(Debug, Clone)]
pub struct ChatAttachConfig {
    /// Largest file accepted.
    pub max_bytes: u64,
    /// Private directory the checked copies are written to.
    pub outbox: PathBuf,
    /// Refused, with everything beneath them.
    pub denied: Vec<PathBuf>,
    /// Allowed even beneath a denied path, such as the media chat users sent,
    /// which lives in the SCV instance.
    pub allowed: Vec<PathBuf>,
}

impl ChatAttachConfig {
    /// The standard rule: the SCV instance `scv_home` (its settings,
    /// credentials, agent homes, and state) except `allowed`, the credential
    /// and key locations under `home`, and host secrets.
    pub fn standard(
        home: Option<&Path>,
        scv_home: &Path,
        outbox: PathBuf,
        allowed: Vec<PathBuf>,
        max_bytes: u64,
    ) -> Self {
        let mut denied = vec![scv_home.to_path_buf()];
        if let Some(home) = home {
            denied.extend(HOME_SECRETS.iter().map(|path| home.join(path)));
        }
        denied.extend(SYSTEM_SECRETS.iter().map(PathBuf::from));
        Self {
            max_bytes,
            outbox,
            denied,
            allowed,
        }
    }

    /// Check `path` and copy it into the outbox, reporting the copy. The
    /// file is opened without following a final symlink and re-checked
    /// through the open handle, so it cannot be swapped after the check.
    pub fn attach(&self, workspace: &Path, path: &str) -> Result<ReplyAttachment, ToolError> {
        use std::io::Read as _;
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
        let mut attached = self.check(workspace, path)?;
        let mut source = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&attached.path)
            .map_err(|error| ToolError(format!("cannot attach {path}: {error}")))?;
        let metadata = source
            .metadata()
            .map_err(|error| ToolError(format!("cannot attach {path}: {error}")))?;
        if !metadata.is_file() || metadata.len() > self.max_bytes {
            return Err(ToolError(format!("cannot attach {path}: the file changed")));
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.outbox)
            .map_err(|error| ToolError(format!("cannot prepare the chat outbox: {error}")))?;
        let _ = std::fs::set_permissions(&self.outbox, std::fs::Permissions::from_mode(0o700));
        let outbox = std::fs::canonicalize(&self.outbox)
            .map_err(|error| ToolError(format!("cannot prepare the chat outbox: {error}")))?;
        let copy = outbox.join(format!(
            "{}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12],
            attached.name
        ));
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&copy)
            .map_err(|error| ToolError(format!("cannot copy {path}: {error}")))?;
        let copied = std::io::copy(&mut (&mut source).take(self.max_bytes + 1), &mut target)
            .map_err(|error| ToolError(format!("cannot copy {path}: {error}")))?;
        if copied > self.max_bytes {
            let _ = std::fs::remove_file(&copy);
            return Err(ToolError(format!("cannot attach {path}: the file changed")));
        }
        attached.path = copy.display().to_string();
        attached.size = copied;
        Ok(attached)
    }

    /// Check `path` (relative paths resolve from `workspace`) and describe
    /// the file to send.
    pub fn check(&self, workspace: &Path, path: &str) -> Result<ReplyAttachment, ToolError> {
        let requested = Path::new(path);
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            workspace.join(requested)
        };
        let resolved = std::fs::canonicalize(&joined)
            .map_err(|error| ToolError(format!("cannot attach {path}: {error}")))?;
        if self.is_denied(&resolved) || has_secret_name(&resolved) {
            return Err(ToolError(format!(
                "cannot attach {path}: it is in a location that holds credentials or keys"
            )));
        }
        let metadata = std::fs::metadata(&resolved)
            .map_err(|error| ToolError(format!("cannot attach {path}: {error}")))?;
        if !metadata.is_file() {
            return Err(ToolError(format!(
                "cannot attach {path}: not a regular file"
            )));
        }
        if metadata.len() == 0 {
            return Err(ToolError(format!(
                "cannot attach {path}: the file is empty"
            )));
        }
        if metadata.len() > self.max_bytes {
            return Err(ToolError(format!(
                "cannot attach {path}: {} bytes is over the {} byte limit",
                metadata.len(),
                self.max_bytes
            )));
        }
        let name = resolved.file_name().map_or_else(
            || "file".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        Ok(ReplyAttachment {
            path: resolved.display().to_string(),
            name,
            mime: String::new(),
            size: metadata.len(),
            caption: String::new(),
        })
    }

    fn is_denied(&self, resolved: &Path) -> bool {
        let under = |roots: &[PathBuf]| {
            roots.iter().any(|root| {
                resolved.starts_with(root)
                    || std::fs::canonicalize(root).is_ok_and(|root| resolved.starts_with(root))
            })
        };
        under(&self.denied) && !under(&self.allowed)
    }
}

/// File names that are secrets wherever they are.
fn has_secret_name(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        value == ".env"
            || value.starts_with(".env.")
            || value.contains("credential")
            || value.contains("private_key")
            || value.ends_with(".pem")
            || value.ends_with(".key")
            || value.ends_with(".p12")
            || value.ends_with(".pfx")
            || value.ends_with(".kdbx")
            || value.starts_with("id_rsa")
            || value.starts_with("id_ecdsa")
            || value.starts_with("id_ed25519")
            || value.starts_with("id_dsa")
    })
}

pub struct ChatAttachTool {
    pub config: ChatAttachConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    #[serde(default)]
    caption: Option<String>,
}

fn parse(arguments: &Value) -> Result<Args, ToolError> {
    let args: Args = serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError(format!("invalid chat_attach arguments: {error}")))?;
    if args.path.trim().is_empty() {
        return Err(ToolError("path must not be empty".into()));
    }
    if args
        .caption
        .as_ref()
        .is_some_and(|caption| caption.len() > MAX_CAPTION_BYTES)
    {
        return Err(ToolError(format!(
            "caption is longer than {MAX_CAPTION_BYTES} bytes"
        )));
    }
    Ok(args)
}

#[async_trait]
impl Tool for ChatAttachTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: CHAT_ATTACH_TOOL.into(),
            description: format!(
                "Send a file to the user in this chat, such as an image, a PDF, or a log, after \
                 your reply text. Use it when the user asks for a file or a picture says more \
                 than words. Images arrive as pictures, anything else as a file. The file must \
                 be a regular file of at most {} MiB; files in credential or key locations are \
                 refused. Call it once per file.",
                self.config.max_bytes / (1024 * 1024)
            ),
            parameters: json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"Absolute path, or a path relative to the workspace"},
                    "caption":{"type":"string","description":"Short text sent with the file"}
                },
                "required":["path"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        parse(arguments)?;
        // The file leaves the host.
        Ok(ToolRisk::Network)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args = parse(arguments)?;
        Ok(format!("Send {} to the chat", args.path))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse(&arguments)?;
        let config = self.config.clone();
        let workspace = context.workspace.clone();
        let path = args.path.clone();
        let mut attached = tokio::task::spawn_blocking(move || config.attach(&workspace, &path))
            .await
            .map_err(|error| ToolError(format!("chat_attach failed: {error}")))??;
        attached.caption = args.caption.unwrap_or_default().trim().to_owned();
        Ok(ToolOutput::success(
            json!({
                "attached": attached,
                "note": "The file is sent after your reply text."
            })
            .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests;
