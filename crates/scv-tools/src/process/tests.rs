//! Unit tests for `src/process.rs`.

use std::collections::HashMap;

use super::*;

#[test]
fn agent_environment_drops_inherited_credentials_but_keeps_its_own_home() {
    let mut command = std::process::Command::new("true");
    apply_agent_environment_from(
        &mut command,
        [
            "GROK_HOME",
            "XAI_API_KEY",
            "PI_CODING_AGENT_DIR",
            "DEEPSEEK_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENROUTER_API_KEY",
            "PATH",
        ]
        .map(OsString::from),
        &[("GROK_HOME".into(), "/private/.grok".into())],
    );
    let envs: HashMap<_, _> = command
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
        .collect();
    assert_eq!(
        envs[&OsString::from("GROK_HOME")],
        Some(OsString::from("/private/.grok"))
    );
    for removed in [
        "XAI_API_KEY",
        "PI_CODING_AGENT_DIR",
        "DEEPSEEK_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENROUTER_API_KEY",
    ] {
        assert_eq!(envs[&OsString::from(removed)], None, "{removed}");
    }
    assert!(!envs.contains_key(&OsString::from("PATH")));
}

#[test]
fn process_groups_zero_and_one_are_never_signalled() {
    // Group 0 is this process's own group and 1 is init's; an ID that does
    // not fit a pid_t cannot name a group at all.
    assert_eq!(ProcessGroup::new(0), None);
    assert_eq!(ProcessGroup::new(1), None);
    assert_eq!(ProcessGroup::new(u32::MAX), None);
    assert!(ProcessGroup::new(2).is_some());
}
