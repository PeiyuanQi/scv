//! Short status lines from a delegated CLI's structured events.
//!
//! Each line names what the agent did (a command, a file it changed, a
//! search, a tool it called), never the output it got back. Commands, URLs,
//! and queries pass through [`redact`] first, so values that look like
//! credentials are replaced before a line leaves the process.

use serde_json::Value;

use crate::delegate::adapters::OutputFormat;

/// Longest command, path, query, or text excerpt in one line, in characters.
const DETAIL_CHARS: usize = 120;

/// The status lines one parsed event contributes, possibly none.
pub(crate) fn progress_lines(format: OutputFormat, kind: &str, event: &Value) -> Vec<String> {
    match format {
        OutputFormat::Text => Vec::new(),
        OutputFormat::ClaudeStreamJson | OutputFormat::AmpStreamJson => claude(kind, event, true),
        OutputFormat::CursorStreamJson => {
            let mut lines = claude(kind, event, true);
            lines.extend(cursor(kind, event));
            lines
        }
        OutputFormat::CodexJsonl => codex(kind, event),
        OutputFormat::PiJson => pi(kind, event),
        OutputFormat::GeminiStreamJson => gemini(kind, event),
    }
}

/// The lines naming tool calls and commands one parsed event contributes,
/// without the message text a status line may quote: the activity log
/// records that text whole.
pub(crate) fn tool_lines(format: OutputFormat, kind: &str, event: &Value) -> Vec<String> {
    match format {
        OutputFormat::ClaudeStreamJson | OutputFormat::AmpStreamJson => claude(kind, event, false),
        OutputFormat::CursorStreamJson => {
            let mut lines = claude(kind, event, false);
            lines.extend(cursor(kind, event));
            lines
        }
        _ => progress_lines(format, kind, event),
    }
}

/// Cursor CLI's `tool_call` events, as each starts.
fn cursor(kind: &str, event: &Value) -> Vec<String> {
    if kind != "tool_call" || event.get("subtype").and_then(Value::as_str) != Some("started") {
        return Vec::new();
    }
    let Some((key, call)) = event
        .get("tool_call")
        .and_then(Value::as_object)
        .and_then(|calls| calls.iter().next())
    else {
        return Vec::new();
    };
    let args = call.get("args").unwrap_or(&Value::Null);
    let field = |name: &str| args.get(name).and_then(Value::as_str);
    let name = key.strip_suffix("ToolCall").unwrap_or(key);
    let line = match name {
        "shell" => field("command").map(shell),
        "read" | "write" | "edit" | "delete" => field("path").map(|path| {
            let mut title = name.to_owned();
            title[..1].make_ascii_uppercase();
            format!("{title} {}", short_path(path))
        }),
        "grep" | "glob" => field("pattern")
            .or_else(|| field("globPattern"))
            .map(|pattern| format!("{name} {}", detail(pattern))),
        _ => None,
    };
    vec![line.unwrap_or_else(|| detail(name))]
}

/// Gemini CLI's `tool_use` and failed `tool_result` events.
fn gemini(kind: &str, event: &Value) -> Vec<String> {
    let name = event
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    match kind {
        "tool_use" => {
            let parameters = event.get("parameters").unwrap_or(&Value::Null);
            let field = |key: &str| parameters.get(key).and_then(Value::as_str);
            let line = match name {
                "run_shell_command" => field("command").map(shell),
                "read_file" | "write_file" | "replace" | "read_many_files" => field("file_path")
                    .or_else(|| field("absolute_path"))
                    .or_else(|| field("path"))
                    .map(|path| format!("{name} {}", short_path(path))),
                "google_web_search" => {
                    field("query").map(|query| format!("search: {}", detail(query)))
                }
                "web_fetch" => field("prompt")
                    .or_else(|| field("url"))
                    .map(|url| format!("fetch {}", url_without_query(url))),
                "glob" | "search_file_content" => {
                    field("pattern").map(|pattern| format!("{name} {}", detail(pattern)))
                }
                _ => None,
            };
            vec![line.unwrap_or_else(|| detail(name))]
        }
        "tool_result" if event.get("status").and_then(Value::as_str) == Some("error") => {
            vec![format!("{} failed", detail(name))]
        }
        _ => Vec::new(),
    }
}

fn claude(kind: &str, event: &Value, with_text: bool) -> Vec<String> {
    if kind != "assistant" {
        return Vec::new();
    }
    let Some(parts) = event.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    parts
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let name = part.get("name").and_then(Value::as_str)?;
                Some(claude_tool(name, part.get("input").unwrap_or(&Value::Null)))
            }
            Some("text") if with_text => {
                let text = part.get("text").and_then(Value::as_str)?;
                let first = text.lines().find(|line| !line.trim().is_empty())?;
                Some(detail(first))
            }
            _ => None,
        })
        .filter(|line| !line.is_empty())
        .collect()
}

fn claude_tool(name: &str, input: &Value) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    match name {
        "Bash" => field("command").map_or_else(|| name.to_owned(), shell),
        "Read" | "Write" | "Edit" | "MultiEdit" => field("file_path").map_or_else(
            || name.to_owned(),
            |path| format!("{name} {}", short_path(path)),
        ),
        "NotebookEdit" => field("notebook_path").map_or_else(
            || name.to_owned(),
            |path| format!("{name} {}", short_path(path)),
        ),
        "Glob" | "Grep" => field("pattern").map_or_else(
            || name.to_owned(),
            |pattern| format!("{name} {}", detail(pattern)),
        ),
        "WebSearch" => field("query").map_or_else(
            || name.to_owned(),
            |query| format!("search: {}", detail(query)),
        ),
        "WebFetch" => field("url").map_or_else(
            || name.to_owned(),
            |url| format!("fetch {}", url_without_query(url)),
        ),
        "Task" | "Agent" => field("description").map_or_else(
            || name.to_owned(),
            |text| format!("{name}: {}", detail(text)),
        ),
        // Plan bookkeeping, not work.
        "TodoWrite" => String::new(),
        _ => name.to_owned(),
    }
}

fn codex(kind: &str, event: &Value) -> Vec<String> {
    let Some(item) = event.get("item") else {
        return Vec::new();
    };
    let field = |key: &str| item.get(key).and_then(Value::as_str);
    let line = match (kind, field("type").unwrap_or("")) {
        ("item.started", "command_execution") => field("command").map(shell),
        ("item.completed", "command_execution") => item
            .get("exit_code")
            .and_then(Value::as_i64)
            .filter(|code| *code != 0)
            .map(|code| match field("command") {
                Some(command) => format!("exit {code}: {}", strip_shell(command)),
                None => format!("exit {code}"),
            }),
        ("item.completed", "file_change") => {
            let changes = item.get("changes").and_then(Value::as_array);
            let lines: Vec<String> = changes
                .into_iter()
                .flatten()
                .filter_map(|change| {
                    let path = change.get("path").and_then(Value::as_str)?;
                    let action = change.get("kind").and_then(Value::as_str).unwrap_or("edit");
                    Some(format!("{action} {}", short_path(path)))
                })
                .collect();
            return lines;
        }
        ("item.completed", "web_search") => field("query")
            .filter(|query| !query.trim().is_empty())
            .map(|query| format!("search: {}", detail(query))),
        ("item.started", "mcp_tool_call") => match (field("server"), field("tool")) {
            (Some(server), Some(tool)) => Some(format!("{server}.{tool}")),
            (_, Some(tool)) => Some(tool.to_owned()),
            _ => None,
        },
        _ => None,
    };
    line.into_iter().collect()
}

fn pi(kind: &str, event: &Value) -> Vec<String> {
    let name = event
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    match kind {
        "tool_execution_start" => {
            let args = event.get("args").unwrap_or(&Value::Null);
            let field = |key: &str| args.get(key).and_then(Value::as_str);
            let line = match name {
                "bash" => field("command").map(shell),
                "read" | "write" | "edit" => field("path")
                    .or_else(|| field("file_path"))
                    .map(|path| format!("{name} {}", short_path(path))),
                "grep" | "find" => {
                    field("pattern").map(|pattern| format!("{name} {}", detail(pattern)))
                }
                _ => None,
            };
            vec![line.unwrap_or_else(|| name.to_owned())]
        }
        "tool_execution_end" if event.get("isError").and_then(Value::as_bool) == Some(true) => {
            vec![format!("{name} failed")]
        }
        _ => Vec::new(),
    }
}

/// `$ <command>` without the login-shell wrapper the CLI adds.
fn shell(command: &str) -> String {
    format!("$ {}", strip_shell(command))
}

/// The inner command of `<shell> -lc '<command>'`, redacted and bounded.
fn strip_shell(command: &str) -> String {
    let trimmed = command.trim();
    let inner = trimmed
        .split_once(' ')
        .filter(|(program, _)| {
            let base = program.rsplit('/').next().unwrap_or(program);
            matches!(base, "bash" | "zsh" | "sh" | "dash")
        })
        .and_then(|(_, rest)| {
            let rest = rest.trim_start();
            rest.strip_prefix("-lc ")
                .or_else(|| rest.strip_prefix("-c "))
                .map(str::trim)
        })
        .map_or(trimmed, |rest| {
            for quote in ['\'', '"'] {
                if let Some(unquoted) = rest
                    .strip_prefix(quote)
                    .and_then(|value| value.strip_suffix(quote))
                {
                    return unquoted;
                }
            }
            rest
        });
    detail(inner)
}

/// The last two components of a path, enough to recognise the file.
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let tail = if parts.len() > 2 {
        format!("…/{}", parts[parts.len() - 2..].join("/"))
    } else {
        path.to_owned()
    };
    detail(&tail)
}

/// A URL's scheme, host, and path; the query and fragment can carry tokens.
fn url_without_query(url: &str) -> String {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    detail(&url[..end])
}

/// Redacted, single-line, and at most `DETAIL_CHARS` characters.
fn detail(text: &str) -> String {
    let line = redact(&text.split_whitespace().collect::<Vec<_>>().join(" "));
    if line.chars().count() <= DETAIL_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(DETAIL_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// Words that name a credential in `name=value` or `name: value` form.
const SECRET_NAMES: [&str; 8] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
    "credential",
    "cookie",
];

/// Prefixes of well-known credential formats.
const SECRET_PREFIXES: [&str; 9] = [
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
];

/// `text` as one display line: whitespace runs collapsed, every URL without
/// its query or fragment (which can carry tokens), and credentials
/// [`redact`]ed. For lines SCV keeps, such as activity logs, whatever
/// produced them.
pub(crate) fn scrub(text: &str) -> String {
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|word| {
            if word.contains("://") {
                &word[..word.find(['?', '#']).unwrap_or(word.len())]
            } else {
                word
            }
        })
        .collect();
    redact(&words.join(" "))
}

/// Replace values that look like credentials with `…`. A heuristic for
/// display lines, not a guarantee: it covers `Bearer <token>`,
/// `NAME=value` and `--name value` where the name mentions a key, token,
/// secret, or password, and well-known token prefixes.
pub(crate) fn redact(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut kept = 0;
    for range in secret_ranges(text) {
        output.push_str(&text[kept..range.start]);
        output.push('…');
        kept = range.end;
    }
    output.push_str(&text[kept..]);
    output
}

/// Where [`redact`] hides something in `text`, as byte ranges in order:
/// each is replaced by `…` there.
pub(crate) fn secret_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    secret_ranges_after(text, false).0
}

/// [`secret_ranges`] of text that may go on from earlier text: with
/// `continued`, its first word is the value a name or auth scheme at the
/// earlier text's end was waiting for. Also returns whether `text` itself
/// ends with such a name or scheme.
fn secret_ranges_after(text: &str, continued: bool) -> (Vec<std::ops::Range<usize>>, bool) {
    let mut ranges = Vec::new();
    let mut hide_next = continued;
    let mut start = 0;
    for word in text.split(' ') {
        let end = start + word.len();
        let lower = word.to_ascii_lowercase();
        let bare = lower.trim_matches(|character: char| {
            matches!(character, '"' | '\'' | '-' | ':' | '(' | ')' | ',')
        });
        let token = word.trim_matches(|character: char| matches!(character, '"' | '\''));
        // An auth scheme is shown; the credential after it is not.
        if bare == "bearer" || bare == "basic" {
            hide_next = true;
        } else if word.is_empty() || (hide_next && !word.chars().any(char::is_alphanumeric)) {
            // More spaces before the value, as a terminal's columns have,
            // or a frame a UI draws there (`│`): the value is still next.
        } else if std::mem::take(&mut hide_next) {
            ranges.push(start..end);
        } else if let Some((name, value)) = word.split_once(['=', ':'])
            && !value.starts_with("//")
            && SECRET_NAMES
                .iter()
                .any(|secret| name.to_ascii_lowercase().contains(secret))
        {
            if value.trim_matches(['"', '\'']).is_empty() {
                // `Authorization: <value>`: the value is the next word.
                hide_next = true;
            } else {
                // The name and its one-byte separator stay.
                ranges.push(start + name.len() + 1..end);
            }
        } else if word.starts_with("--") && SECRET_NAMES.iter().any(|secret| bare.contains(secret))
        {
            hide_next = true;
        } else if SECRET_PREFIXES
            .iter()
            .any(|prefix| token.starts_with(prefix) && token.len() >= prefix.len() + 8)
        {
            ranges.push(start..end);
        }
        start = end + 1;
    }
    (ranges, hide_next)
}

/// What [`scrub`] would drop or hide in `text`, without collapsing its
/// spacing, as byte ranges in order: every URL's query and fragment, and
/// what [`redact`] hides. For text whose layout must stay, such as a
/// terminal's screen, read a line at a time: with `continued`, the line
/// before ended with a name or auth scheme whose value is this line's first
/// word, or with a hidden value a UI wrapped onto this line. Also returns
/// whether this line ends with a name or scheme still waiting for its value.
pub(crate) fn scrubbed_ranges(text: &str, continued: bool) -> (Vec<std::ops::Range<usize>>, bool) {
    let (mut ranges, waiting) = secret_ranges_after(text, continued);
    let mut start = 0;
    for word in text.split(' ') {
        if word.contains("://")
            && let Some(query) = word.find(['?', '#'])
        {
            ranges.push(start + query..start + word.len());
        }
        start += word.len() + 1;
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    (merged, waiting)
}

#[cfg(test)]
mod tests;
