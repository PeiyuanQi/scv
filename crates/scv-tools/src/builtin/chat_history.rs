//! `chat_history` and `chat_keep`: the chat log of the conversation a chat
//! session answers (see [`scv_client::history`]).
//!
//! `chat_history` lists, reads, and searches the conversation's earlier
//! episodes, read-only. `chat_keep` moves a file the user sent out of the
//! chat media directory, whose files expire, into the conversation's kept
//! files. Both work only on this conversation's own directories.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use scv_client::history::{self, Entry, FileRef};
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRisk, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::args::parse_args;

/// Most results one call returns, and the default.
const MAX_LIMIT: usize = 100;
const DEFAULT_SEARCH: usize = 10;
const DEFAULT_EPISODES: usize = 20;
const DEFAULT_READ: usize = 30;
/// Most of the log one search reads.
const MAX_SEARCH_BYTES: u64 = 64 * 1024 * 1024;
/// Longest message text one `read` shows, in characters.
const MAX_MESSAGE_CHARS: usize = 8000;

/// The conversation a chat session answers: its log, where its received
/// files are saved, and where kept files go.
#[derive(Debug, Clone)]
pub struct ChatHistoryConfig {
    pub log: PathBuf,
    pub media: PathBuf,
    pub kept: PathBuf,
    /// Largest result, in bytes.
    pub output_limit: usize,
}

impl ChatHistoryConfig {
    /// The conversation `conversation` (a path from
    /// [`history::conversation_path`]) under the history, media, and archive
    /// roots.
    pub fn new(
        history_root: &Path,
        media_root: &Path,
        archive_root: &Path,
        conversation: &Path,
        output_limit: usize,
    ) -> Self {
        Self {
            log: history_root.join(conversation),
            media: media_root.join(conversation),
            kept: history::kept_dir(archive_root, conversation),
            output_limit,
        }
    }
}

pub(crate) struct ChatHistoryTool {
    pub(crate) config: ChatHistoryConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryArgs {
    action: String,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    episode: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

fn is_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

impl HistoryArgs {
    fn check(&self) -> Result<(), ToolError> {
        match self.action.as_str() {
            "search" if self.query.as_deref().is_none_or(|q| q.trim().is_empty()) => Err(
                ToolError::invalid_arguments("search needs a non-empty query"),
            ),
            "read" if self.episode.is_none() => Err(ToolError::invalid_arguments(
                "read needs an episode ID from episodes or search",
            )),
            "search" | "read" | "episodes" => {
                for date in [&self.before, &self.after].into_iter().flatten() {
                    if !is_date(date) {
                        return Err(ToolError::invalid_arguments(
                            "before and after are dates like 2026-09-26",
                        ));
                    }
                }
                Ok(())
            }
            _ => Err(ToolError::invalid_arguments(
                "action must be episodes, read, or search",
            )),
        }
    }

    fn limit(&self, default: usize) -> usize {
        self.limit.unwrap_or(default).clamp(1, MAX_LIMIT)
    }
}

#[async_trait]
impl Tool for ChatHistoryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "chat_history".into(),
            description: "Look back through this chat's log, which keeps every earlier \
                conversation with this user, grouped into episodes (runs of messages without \
                a long pause). `search` finds messages containing all words of `query`, \
                newest first; `episodes` lists episodes newest first, optionally only those \
                started `before` or on/`after` a date (YYYY-MM-DD); `read` shows one episode's \
                messages from `offset`. Use it when the user refers to something that is not \
                in this conversation, instead of guessing or asking them to repeat it."
                .into(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "action":{"type":"string","enum":["search","episodes","read"]},
                    "query":{"type":"string","description":"Words to find (search)"},
                    "episode":{"type":"string","description":"Episode ID from episodes or search (read)"},
                    "before":{"type":"string","description":"Only episodes started before this date, YYYY-MM-DD (episodes)"},
                    "after":{"type":"string","description":"Only episodes started on or after this date, YYYY-MM-DD (episodes)"},
                    "offset":{"type":"integer","minimum":0,"description":"First message to show (read); search hits give each message's index"},
                    "limit":{"type":"integer","minimum":1,"maximum":MAX_LIMIT}
                },
                "required":["action"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: HistoryArgs = parse_args(arguments)?;
        args.check()?;
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: HistoryArgs = parse_args(arguments)?;
        Ok(match args.action.as_str() {
            "search" => format!(
                "Search this chat's history for {:?}",
                args.query.unwrap_or_default()
            ),
            "read" => format!(
                "Read chat history episode {}",
                args.episode.unwrap_or_default()
            ),
            _ => "List this chat's history episodes".into(),
        })
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: HistoryArgs = parse_args(&arguments)?;
        args.check()?;
        let config = self.config.clone();
        tokio::select! {
            result = tokio::task::spawn_blocking(move || run(&config, &args)) => {
                result.map_err(|error| ToolError::failed(format!("chat history task failed: {error}")))?
            }
            () = context.cancellation.cancelled() => Err(ToolError::cancelled("chat history cancelled")),
        }
    }
}

fn failed(error: &std::io::Error) -> ToolError {
    ToolError::failed(format!("cannot read the chat history: {error}"))
}

fn run(config: &ChatHistoryConfig, args: &HistoryArgs) -> Result<ToolOutput, ToolError> {
    let value = match args.action.as_str() {
        "search" => {
            let query = args.query.as_deref().unwrap_or_default();
            let (hits, more) = history::search(
                &config.log,
                query,
                args.limit(DEFAULT_SEARCH),
                MAX_SEARCH_BYTES,
            )
            .map_err(|error| failed(&error))?;
            json!({"hits":hits,"more":more})
        }
        "episodes" => {
            let (episodes, more) = history::episodes(
                &config.log,
                args.before.as_deref(),
                args.after.as_deref(),
                args.limit(DEFAULT_EPISODES),
            )
            .map_err(|error| failed(&error))?;
            json!({"episodes":episodes,"more":more})
        }
        _ => {
            let id = args.episode.as_deref().unwrap_or_default();
            let offset = args.offset.unwrap_or(0);
            let Some((messages, total)) =
                history::read_episode(&config.log, id, offset, args.limit(DEFAULT_READ))
                    .map_err(|error| failed(&error))?
            else {
                return Err(ToolError::invalid_arguments(format!(
                    "no episode {id}; list them with action episodes"
                )));
            };
            let messages: Vec<Value> = messages
                .iter()
                .enumerate()
                .map(|(index, entry)| describe(config, offset + index, entry))
                .collect();
            json!({"episode":id,"offset":offset,"total":total,"messages":messages})
        }
    };
    Ok(bounded(value, config.output_limit))
}

/// A message as `read` shows it, with each file's current place: still in
/// the chat media, kept, or gone after its retention.
fn describe(config: &ChatHistoryConfig, index: usize, entry: &Entry) -> Value {
    let mut text: String = entry.text.chars().take(MAX_MESSAGE_CHARS).collect();
    let more = entry.text.chars().count().saturating_sub(MAX_MESSAGE_CHARS);
    if more > 0 {
        text.push_str(&format!("… [{more} more characters]"));
    }
    let mut message = json!({
        "index":index,
        "local":entry.local,
        "role":entry.role,
        "text":text,
    });
    if !entry.quote.is_empty() {
        message["quote"] = json!(
            entry
                .quote
                .chars()
                .take(MAX_MESSAGE_CHARS)
                .collect::<String>()
        );
    }
    if !entry.files.is_empty() {
        message["files"] = entry
            .files
            .iter()
            .map(|file| describe_file(config, entry.role, file))
            .collect();
    }
    if !entry.notes.is_empty() {
        message["notes"] = json!(entry.notes);
    }
    if entry.report {
        message["report"] = json!(true);
    }
    message
}

fn describe_file(config: &ChatHistoryConfig, role: history::Role, file: &FileRef) -> Value {
    let mut described = json!({"kind":file.kind,"name":file.name});
    if !file.transcript.is_empty() {
        described["transcript"] = json!(file.transcript);
    }
    if file.path.is_empty() {
        // SCV keeps no copy of what it sent, and a file the user sent that
        // has no path was never saved.
        let key = if role == history::Role::Owner {
            "not_saved"
        } else {
            "sent"
        };
        described[key] = json!(true);
        return described;
    }
    let saved = Path::new(&file.path);
    let kept = saved.file_name().map(|name| config.kept.join(name));
    let in_media = saved.canonicalize().is_ok_and(|saved| {
        config
            .media
            .canonicalize()
            .is_ok_and(|media| saved.starts_with(media))
    });
    if in_media && saved.is_file() {
        described["path"] = json!(file.path);
    } else if let Some(kept) = kept.filter(|kept| kept.is_file()) {
        described["path"] = json!(kept.display().to_string());
        described["kept"] = json!(true);
    } else {
        described["gone"] = json!(true);
    }
    described
}

/// `value` as JSON of at most `limit` bytes: list results lose their tail
/// until they fit, with `more` set.
fn bounded(mut value: Value, limit: usize) -> ToolOutput {
    let mut truncated = false;
    loop {
        let content = value.to_string();
        if content.len() <= limit {
            return ToolOutput {
                content,
                failure: None,
                truncated,
            };
        }
        let key = ["hits", "episodes", "messages"]
            .into_iter()
            .find(|key| value.get(key).is_some_and(Value::is_array));
        match key
            .and_then(|key| value.get_mut(key))
            .and_then(Value::as_array_mut)
        {
            Some(list) if list.len() > 1 => {
                list.truncate(list.len() / 2);
                value["more"] = json!(true);
                truncated = true;
            }
            _ => {
                let content = scv_client::text::utf8_prefix(&content, limit).to_owned();
                return ToolOutput {
                    content,
                    failure: None,
                    truncated: true,
                };
            }
        }
    }
}

pub(crate) struct ChatKeepTool {
    pub(crate) config: ChatHistoryConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeepArgs {
    path: String,
}

#[async_trait]
impl Tool for ChatKeepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "chat_keep".into(),
            description: "Keep a file the user sent in this chat for good. Files from chat are \
                removed after a while; when the user asks to keep one (\"keep this\"), pass the \
                path shown with it, and it moves to this chat's kept files, which are never \
                removed. chat_history then shows it at its new path."
                .into(),
            parameters: json!({
                "type":"object",
                "properties":{"path":{"type":"string","description":"The file's path as shown with the message"}},
                "required":["path"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let _: KeepArgs = parse_args(arguments)?;
        Ok(ToolRisk::Filesystem)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: KeepArgs = parse_args(arguments)?;
        Ok(format!("Keep the chat file {} for good", args.path))
    }

    async fn execute(
        &self,
        arguments: Value,
        _context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: KeepArgs = parse_args(&arguments)?;
        let config = self.config.clone();
        let kept = tokio::task::spawn_blocking(move || keep(&config, &args.path))
            .await
            .map_err(|error| ToolError::failed(format!("chat keep task failed: {error}")))??;
        Ok(ToolOutput {
            content: json!({"kept":kept.display().to_string()}).to_string(),
            failure: None,
            truncated: false,
        })
    }
}

/// Move a received file of this conversation to its kept files. Keeping a
/// file already kept reports where it is.
fn keep(config: &ChatHistoryConfig, path: &str) -> Result<PathBuf, ToolError> {
    let refuse = || {
        ToolError::failed(format!(
            "cannot keep {path}: only files received in this chat can be kept"
        ))
    };
    let requested = Path::new(path);
    if !requested.is_absolute() {
        return Err(refuse());
    }
    let name = requested.file_name().ok_or_else(refuse)?;
    let kept = config.kept.join(name);
    if requested.parent().and_then(|dir| dir.canonicalize().ok()) == config.kept.canonicalize().ok()
        && kept.is_file()
    {
        return Ok(kept);
    }
    let media = config.media.canonicalize().map_err(|_| refuse())?;
    let parent = requested
        .parent()
        .and_then(|dir| dir.canonicalize().ok())
        .ok_or_else(refuse)?;
    let metadata = std::fs::symlink_metadata(requested).map_err(|_| refuse())?;
    if parent != media || !metadata.is_file() {
        return Err(refuse());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    let failed = |error: std::io::Error| ToolError::failed(format!("cannot keep {path}: {error}"));
    builder.create(&config.kept).map_err(failed)?;
    let taken = || {
        ToolError::failed(format!(
            "cannot keep {path}: a kept file has that name already"
        ))
    };
    let source = media.join(name);
    // A link never replaces an existing name, so a kept file is never
    // overwritten, and the file is in both places until the original goes.
    match std::fs::hard_link(&source, &kept) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Err(taken()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            copy_across(&source, &kept).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    taken()
                } else {
                    failed(error)
                }
            })?;
        }
        Err(error) => return Err(failed(error)),
    }
    let _ = std::fs::remove_file(&source);
    Ok(kept)
}

/// Copy `source` to `kept` on another disk: into a private temporary file
/// beside `kept`, then linked into place without replacing anything, so a
/// failed copy leaves no partial file under the kept name.
fn copy_across(source: &Path, kept: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = kept
        .parent()
        .ok_or_else(|| std::io::Error::other("kept files have no directory"))?;
    let temporary = dir.join(format!(".keep-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut from = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        let mut to = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        std::io::copy(&mut from, &mut to)?;
        to.flush()?;
        to.sync_all()?;
        std::fs::hard_link(&temporary, kept)
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

#[cfg(test)]
mod tests;
