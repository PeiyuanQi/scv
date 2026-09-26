//! The `read` and `write` tools: bounded, UTF-8, and confined to the
//! workspace through a capability handle, so symlinks cannot escape it.

use std::{
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::args::parse_args;

pub(crate) struct ReadTool {
    pub(crate) max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a bounded UTF-8 file inside the workspace".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "offset":{"type":"integer","minimum":0},
                    "limit":{"type":"integer","minimum":1}
                },
                "required":["path"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: ReadArgs = parse_args(arguments)?;
        validate_read_args(&args)?;
        Ok(if is_secret_like(Path::new(&args.path)) {
            ToolRisk::Filesystem
        } else {
            ToolRisk::ReadOnly
        })
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: ReadArgs = parse_args(arguments)?;
        validate_read_args(&args)?;
        Ok(format!("Read {}", args.path))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: ReadArgs = parse_args(&arguments)?;
        validate_read_args(&args)?;
        let requested = args.limit.unwrap_or(self.max_bytes).min(self.max_bytes);
        let offset = u64::try_from(args.offset).unwrap_or(u64::MAX);
        let workspace = context.workspace.clone();
        let display_path = args.path.clone();
        let relative = PathBuf::from(&args.path);
        validate_relative(&relative)?;
        let read = tokio::task::spawn_blocking(move || {
            let root = open_workspace(&workspace)?;
            let mut file = root
                .open(&relative)
                .map_err(|error| map_cap_error("read", &display_path, error))?;
            let total_bytes = file
                .metadata()
                .map_err(|error| ToolError::failed(format!("stat {display_path}: {error}")))?
                .len();
            let start = offset.min(total_bytes);
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start))
                .map_err(|error| ToolError::failed(format!("seek {display_path}: {error}")))?;
            let mut bytes = Vec::with_capacity(requested.min(8192));
            std::io::Read::take(&mut file, u64::try_from(requested).unwrap_or(u64::MAX))
                .read_to_end(&mut bytes)
                .map_err(|error| ToolError::failed(format!("read {display_path}: {error}")))?;
            Ok::<_, ToolError>((bytes, total_bytes, start))
        });
        let (bytes, total_bytes, start) = tokio::select! {
            result = read => result.map_err(|error| ToolError::failed(format!("read task failed: {error}")))??,
            () = context.cancellation.cancelled() => return Err(ToolError::cancelled("read cancelled")),
        };
        let content = std::str::from_utf8(&bytes).map_err(|_| {
            ToolError::failed(format!("selected range of {} is not UTF-8", args.path))
        })?;
        let end = start.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let truncated = start > 0 || end < total_bytes;
        Ok(ToolOutput {
            content: json!({
                "path": args.path,
                "content": content,
                "total_bytes": total_bytes,
                "offset": start,
                "truncated": truncated
            })
            .to_string(),
            failure: None,
            truncated,
        })
    }
}

pub(crate) struct WriteTool {
    pub(crate) max_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    mode: WriteMode,
    expected_sha256: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WriteMode {
    Create,
    Replace,
}

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Atomically create or replace a UTF-8 file inside the workspace".into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "content":{"type":"string"},
                    "mode":{"type":"string","enum":["create","replace"]},
                    "expected_sha256":{"type":"string"}
                },
                "required":["path","content","mode"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let _: WriteArgs = parse_args(arguments)?;
        Ok(ToolRisk::Filesystem)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: WriteArgs = parse_args(arguments)?;
        let mode = match args.mode {
            WriteMode::Create => "Create",
            WriteMode::Replace => "Replace",
        };
        Ok(format!(
            "{mode} {} ({} bytes)",
            args.path,
            args.content.len()
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: WriteArgs = parse_args(&arguments)?;
        if args.content.len() > self.max_bytes {
            return Err(ToolError::limit(format!(
                "write exceeds {} byte limit",
                self.max_bytes
            )));
        }
        let workspace = context.workspace.clone();
        let cancellation = context.cancellation.clone();
        tokio::task::spawn_blocking(move || {
            if cancellation.is_cancelled() {
                return Err(ToolError::cancelled("write cancelled"));
            }
            let path = PathBuf::from(&args.path);
            validate_relative(&path)?;
            let root = open_workspace(&workspace)?;
            let exists = match root.symlink_metadata(&path) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(map_cap_error("inspect", &args.path, error)),
            };
            match args.mode {
                WriteMode::Create if exists => {
                    return Err(ToolError::failed(format!("{} already exists", args.path)));
                }
                WriteMode::Replace if !exists => {
                    return Err(ToolError::failed(format!("{} does not exist", args.path)));
                }
                _ => {}
            }
            if let Some(expected) = args.expected_sha256 {
                let mut current_file = root
                    .open(&path)
                    .map_err(|error| map_cap_error("hash", &args.path, error))?;
                let mut current = Vec::new();
                current_file
                    .read_to_end(&mut current)
                    .map_err(|error| ToolError::failed(format!("hash {}: {error}", args.path)))?;
                let actual = format!("{:x}", Sha256::digest(current));
                if actual != expected.to_ascii_lowercase() {
                    return Err(ToolError::failed(format!(
                        "{} changed: expected sha256 {}, found {}",
                        args.path, expected, actual
                    )));
                }
            }
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            root.create_dir_all(parent)
                .map_err(|error| map_cap_error("create directory for", &args.path, error))?;
            let temporary_path = unique_temporary_path(parent);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut temporary = root
                .open_with(&temporary_path, &options)
                .map_err(|error| map_cap_error("create temporary file for", &args.path, error))?;
            let write_result = (|| {
                temporary
                    .write_all(args.content.as_bytes())
                    .and_then(|()| temporary.sync_all())
                    .map_err(|error| ToolError::failed(format!("write {}: {error}", args.path)))?;
                if cancellation.is_cancelled() {
                    return Err(ToolError::cancelled("write cancelled"));
                }
                match args.mode {
                    WriteMode::Create => root
                        .hard_link(&temporary_path, &root, &path)
                        .map_err(|error| map_cap_error("create", &args.path, error)),
                    WriteMode::Replace => root
                        .rename(&temporary_path, &root, &path)
                        .map_err(|error| map_cap_error("replace", &args.path, error)),
                }
            })();
            if matches!(args.mode, WriteMode::Create) || write_result.is_err() {
                let _ = root.remove_file(&temporary_path);
            }
            write_result?;
            Ok(ToolOutput::success(
                json!({
                    "path":args.path,
                    "bytes":args.content.len(),
                    "sha256":format!("{:x}", Sha256::digest(args.content.as_bytes()))
                })
                .to_string(),
            ))
        })
        .await
        .map_err(|error| ToolError::failed(format!("write task failed: {error}")))?
    }
}

fn validate_read_args(args: &ReadArgs) -> Result<(), ToolError> {
    if args.limit == Some(0) {
        return Err(ToolError::invalid_arguments("read limit must be positive"));
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<(), ToolError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(ToolError::invalid_arguments(
            "path must be non-empty and relative",
        ));
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(ToolError::invalid_arguments(
                "parent traversal and absolute paths are not allowed",
            ));
        }
    }
    Ok(())
}

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

fn open_workspace(workspace: &Path) -> Result<Dir, ToolError> {
    Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|error| ToolError::failed(format!("open workspace capability: {error}")))
}

fn unique_temporary_path(parent: &Path) -> PathBuf {
    let id = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".scv-write-{}-{id}.tmp", std::process::id()))
}

fn map_cap_error(action: &str, path: &str, error: std::io::Error) -> ToolError {
    ToolError::failed(format!(
        "{action} {path}: {error}; path must remain within workspace"
    ))
}

pub(crate) fn is_secret_like(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        value == ".env"
            || value.starts_with(".env.")
            || value.contains("credential")
            || value.contains("private_key")
            || value.ends_with(".pem")
            || value.ends_with(".key")
    })
}

#[cfg(test)]
mod tests;
