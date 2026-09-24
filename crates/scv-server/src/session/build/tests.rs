//! Unit tests for `src/session/build.rs`.

use super::*;

#[test]
fn signed_out_agents_with_a_local_sign_in_check_are_not_offered() {
    let home = tempfile::tempdir().unwrap();
    let config = Config {
        instance_home: home.path().to_owned(),
        ..Config::default()
    };
    let offered = offered_adapters(&config);
    // Nothing stored for dsh, pi, grok, or the nested SCV: all hidden.
    for hidden in ["agent_dsh", "agent_pi", "agent_grok", "agent_scv"] {
        assert!(!offered.contains_key(hidden), "{hidden} offered");
    }
    // Claude and Codex report sign-in through their own CLI, which is
    // too slow to run at every session start, so they stay offered.
    assert!(offered.contains_key("agent_claude"));
    assert!(offered.contains_key("agent_codex"));
    // A stored dsh key makes it available.
    let dsh = home.path().join("agents/dsh/.dsh");
    std::fs::create_dir_all(&dsh).unwrap();
    std::fs::write(
        dsh.join(".credentials.yaml"),
        "version: 1\n\nrefs:\n  DEEPSEEK_API_KEY: test-only\n",
    )
    .unwrap();
    assert!(offered_adapters(&config).contains_key("agent_dsh"));
}
