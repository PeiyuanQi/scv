//! Unit tests for `src/delegate/mode.rs`.

use std::io::Write as _;

use super::*;

fn expected(full: bool) -> Expected {
    Expected {
        agent: "claude".into(),
        policy: if full { "auto" } else { "on-risk" }.into(),
        full_mode: CLAUDE_FULL_MODE.into(),
        full,
    }
}

#[test]
fn auto_needs_the_full_mode_and_any_other_mode_refuses_it() {
    let auto = expected(true);
    assert!(auto.accepts("bypassPermissions"));
    // A managed setting that downgrades bypass shows here.
    assert!(!auto.accepts("default"));
    let error = auto.mismatch("default");
    assert!(
        error.contains("reported permission mode default"),
        "{error}"
    );
    assert!(
        error.contains("policy mode auto expects bypassPermissions"),
        "{error}"
    );
    // An agent home that turned its own prompts off is caught too.
    let on_risk = expected(false);
    assert!(on_risk.accepts("default"));
    assert!(on_risk.accepts("plan"));
    assert!(!on_risk.accepts("bypassPermissions"));
    let error = on_risk.mismatch("bypassPermissions");
    assert!(
        error.contains("expects a mode other than bypassPermissions"),
        "{error}"
    );
    let missing = auto.missing();
    assert!(
        missing.contains("did not report its permission mode"),
        "{missing}"
    );
    assert!(missing.contains("the run was stopped"), "{missing}");
}

#[test]
fn summaries_name_the_policy_and_whether_the_mode_is_verified() {
    let full = summary("auto", true, Check::BeforeTools, false);
    assert!(
        full.contains("FULL PERMISSIONS (policy mode auto;"),
        "{full}"
    );
    assert!(full.contains("before any tool runs"), "{full}");
    let blind = summary("auto", true, Check::Unverified, false);
    assert!(blind.contains("mode unverified"), "{blind}");
    let asks = summary("on-risk", false, Check::BeforePrompt, true);
    assert!(asks.contains("Policy mode on-risk"), "{asks}");
    assert!(asks.contains("come back here for approval"), "{asks}");
    assert!(!asks.contains("FULL PERMISSIONS"), "{asks}");
    let none = summary("always", false, Check::NotApplicable, false);
    assert!(none.contains("no full-autonomy switch"), "{none}");
    assert_eq!(
        unverified(Check::Unverified, None),
        Some("unverified".to_owned())
    );
    assert_eq!(
        unverified(Check::Unverified, Some("default")),
        Some("unverified (it reports default)".to_owned())
    );
    assert_eq!(unverified(Check::NotApplicable, Some("default")), None);
    assert_eq!(verified("bypassPermissions"), "verified bypassPermissions");
}

fn write_line(file: &mut std::fs::File, value: &serde_json::Value) {
    writeln!(file, "{value}").unwrap();
}

#[test]
fn a_codex_rollout_records_the_mode_of_its_latest_turn() {
    let home = tempfile::tempdir().unwrap();
    let thread = "019a0000-aaaa-bbbb-cccc-000000000001";
    assert_eq!(codex_rollout(home.path(), thread), None);
    let day = home.path().join("sessions/2026/10/30");
    std::fs::create_dir_all(&day).unwrap();
    // Another thread's file is not this one's.
    std::fs::write(day.join("rollout-2026-10-30T09-00-00-other.jsonl"), "").unwrap();
    let path = day.join(format!("rollout-2026-10-30T10-00-00-{thread}.jsonl"));
    let mut file = std::fs::File::create(&path).unwrap();
    write_line(
        &mut file,
        &serde_json::json!({"type":"session_meta","payload":{"id":thread}}),
    );
    assert_eq!(codex_rollout(home.path(), thread), Some(path.clone()));
    assert_eq!(codex_turn_mode(&path, 0), None);
    write_line(
        &mut file,
        &serde_json::json!({"type":"turn_context","payload":{
            "approval_policy":"on-request",
            "sandbox_policy":{"type":"workspace-write"}
        }}),
    );
    assert_eq!(
        codex_turn_mode(&path, 0).as_deref(),
        Some("approval_policy on-request, sandbox workspace-write")
    );
    // A continued turn counts only what it appended.
    let length = std::fs::metadata(&path).unwrap().len();
    assert_eq!(codex_turn_mode(&path, length), None);
    write_line(
        &mut file,
        &serde_json::json!({"type":"turn_context","payload":{
            "approval_policy":"never",
            "sandbox_policy":{"type":"danger-full-access"}
        }}),
    );
    assert_eq!(
        codex_turn_mode(&path, length).as_deref(),
        Some(CODEX_FULL_MODE)
    );
    // A line still being written is not read yet.
    write!(
        file,
        r#"{{"type":"turn_context","payload":{{"approval_policy":"on-req"#
    )
    .unwrap();
    assert_eq!(codex_turn_mode(&path, 0).as_deref(), Some(CODEX_FULL_MODE));
}
