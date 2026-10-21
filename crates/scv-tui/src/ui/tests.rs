//! Unit tests for `src/ui.rs`. The dashboard's flows over a server
//! connection are tested in `src/tests.rs`.

use super::*;

#[test]
fn a_job_command_takes_options_then_the_prompt() {
    let job = parse_job("codex --cwd shop --effort high --worktree fix the race now").unwrap();
    assert_eq!(
        job,
        JobCommand {
            agent: "codex".into(),
            prompt: "fix the race now".into(),
            cwd: Some("shop".into()),
            model: None,
            effort: Some("high".into()),
            worktree: true,
        }
    );
    assert_eq!(parse_job("claude review it").unwrap().prompt, "review it");
    // Options only come before the prompt.
    assert_eq!(
        parse_job("codex explain --worktree").unwrap().prompt,
        "explain --worktree"
    );
    for bad in ["", "codex", "codex --cwd", "codex --worktree"] {
        assert!(parse_job(bad).is_err(), "{bad}");
    }
}

#[test]
fn workspaces_resolve_against_the_tab_and_home() {
    let base = Path::new("/w/projects");
    assert_eq!(expand("scv", base), PathBuf::from("/w/projects/scv"));
    assert_eq!(expand("/srv/x", base), PathBuf::from("/srv/x"));
    if let Some(home) = std::env::var_os("HOME") {
        assert_eq!(expand("~/code", base), PathBuf::from(home).join("code"));
    }
}

#[test]
fn a_restored_queue_is_noted_once_with_the_prompt_that_was_running() {
    let mut app = App::new(crate::client::SessionInfo {
        id: "new".into(),
        keys: BTreeMap::new(),
        restored: Some(scv_protocol::RestoredQueue {
            from: "9a0e1f2d-session".into(),
            queued: 2,
            interrupted: Some("Run the release checklist.".into()),
        }),
        cwd: "/w".into(),
        model: "m".into(),
        context_max_tokens: 100,
        max_server_frame_bytes: 65536,
        max_transcript_bytes: 16384,
        max_transcript_items: 100,
        max_prompt_history_bytes: 1024,
        max_prompt_history_items: 10,
    });
    note_restored(&mut app);
    note_restored(&mut app);
    let notes: Vec<&String> = app
        .items
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::System(text) if text.contains("Restored") => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("Restored 2 queued prompt(s) from session 9a0e1f2d"));
    assert!(notes[0].contains("paused"));
    assert!(notes[0].contains("not sent again"));
    assert!(notes[0].contains("Run the release checklist."));
}
