//! Unit tests for `src/delegate/progress.rs`.

use super::*;
use serde_json::json;

#[test]
fn codex_reports_commands_files_and_searches() {
    let started = json!({"type":"item.started","item":{"type":"command_execution","command":"/usr/bin/zsh -lc 'cargo test -p scv-tools'","aggregated_output":"secret output","exit_code":null}});
    assert_eq!(
        progress_lines(OutputFormat::CodexJsonl, "item.started", &started),
        ["$ cargo test -p scv-tools"]
    );
    let failed = json!({"type":"item.completed","item":{"type":"command_execution","command":"bash -lc \"false\"","aggregated_output":"boom","exit_code":1}});
    assert_eq!(
        progress_lines(OutputFormat::CodexJsonl, "item.completed", &failed),
        ["exit 1: false"]
    );
    let succeeded = json!({"type":"item.completed","item":{"type":"command_execution","command":"ls","exit_code":0}});
    assert!(progress_lines(OutputFormat::CodexJsonl, "item.completed", &succeeded).is_empty());
    let files = json!({"type":"item.completed","item":{"type":"file_change","changes":[{"path":"/home/u/projects/scv/src/main.rs","kind":"update"},{"path":"note.txt","kind":"add"}]}});
    assert_eq!(
        progress_lines(OutputFormat::CodexJsonl, "item.completed", &files),
        ["update …/src/main.rs", "add note.txt"]
    );
    let search = json!({"type":"item.completed","item":{"type":"web_search","query":"latest serde version"}});
    assert_eq!(
        progress_lines(OutputFormat::CodexJsonl, "item.completed", &search),
        ["search: latest serde version"]
    );
    let message =
        json!({"type":"item.completed","item":{"type":"agent_message","text":"final answer"}});
    assert!(progress_lines(OutputFormat::CodexJsonl, "item.completed", &message).is_empty());
}

#[test]
fn claude_reports_tool_use_and_brief_text_but_not_results() {
    let assistant = json!({"type":"assistant","message":{"content":[
        {"type":"text","text":"I'll list the files.\nThen more detail."},
        {"type":"tool_use","name":"Bash","input":{"command":"ls -la","description":"List"}},
        {"type":"tool_use","name":"Edit","input":{"file_path":"/w/crates/core/src/lib.rs","old_string":"a","new_string":"b"}},
        {"type":"tool_use","name":"WebFetch","input":{"url":"https://docs.rs/serde?token=abc","prompt":"x"}},
        {"type":"tool_use","name":"TodoWrite","input":{"todos":[]}},
        {"type":"tool_use","name":"mcp__github__search","input":{}}
    ]}});
    assert_eq!(
        progress_lines(OutputFormat::ClaudeStreamJson, "assistant", &assistant),
        [
            "I'll list the files.",
            "$ ls -la",
            "Edit …/src/lib.rs",
            "fetch https://docs.rs/serde",
            "mcp__github__search",
        ]
    );
    let result = json!({"type":"user","message":{"content":[{"type":"tool_result","content":"private output"}]}});
    assert!(progress_lines(OutputFormat::ClaudeStreamJson, "user", &result).is_empty());
}

#[test]
fn pi_reports_tool_starts_and_failures() {
    let bash = json!({"type":"tool_execution_start","toolName":"bash","args":{"command":"ls","timeout":10}});
    assert_eq!(
        progress_lines(OutputFormat::PiJson, "tool_execution_start", &bash),
        ["$ ls"]
    );
    let write = json!({"type":"tool_execution_start","toolName":"write","args":{"path":"pi.txt","content":"hi"}});
    assert_eq!(
        progress_lines(OutputFormat::PiJson, "tool_execution_start", &write),
        ["write pi.txt"]
    );
    let failed = json!({"type":"tool_execution_end","toolName":"bash","result":{"content":[{"type":"text","text":"out"}]},"isError":true});
    assert_eq!(
        progress_lines(OutputFormat::PiJson, "tool_execution_end", &failed),
        ["bash failed"]
    );
    let update = json!({"type":"tool_execution_update","toolName":"bash","partialResult":{"content":[{"type":"text","text":"out"}]}});
    assert!(progress_lines(OutputFormat::PiJson, "tool_execution_update", &update).is_empty());
}

#[test]
fn credentials_are_redacted() {
    assert_eq!(
        redact("curl -H Authorization: Bearer abc.def https://x"),
        "curl -H Authorization: Bearer … https://x"
    );
    assert_eq!(
        redact("OPENAI_API_KEY=sk-live-123 run"),
        "OPENAI_API_KEY=… run"
    );
    assert_eq!(
        redact("tool --api-key s3cret --verbose"),
        "tool --api-key … --verbose"
    );
    assert_eq!(
        redact("echo sk-abcdefghijklmnop ghp_0123456789abcdef"),
        "echo … …"
    );
    assert_eq!(redact("cargo test -p scv-tools"), "cargo test -p scv-tools");
    assert_eq!(redact("git log --oneline -3"), "git log --oneline -3");
    assert_eq!(
        codex(
            "item.started",
            &json!({"item":{"type":"command_execution","command":"bash -lc 'curl -H \"Authorization: Bearer tok123\" https://api'"}})
        ),
        ["$ curl -H \"Authorization: Bearer … https://api"]
    );
}

#[test]
fn details_are_one_bounded_line() {
    let long = format!("echo {}", "x".repeat(400));
    let line = shell(&long);
    assert!(line.chars().count() <= DETAIL_CHARS + 2);
    assert!(line.ends_with('…'));
    assert_eq!(detail("a\n  b\tc"), "a b c");
}
