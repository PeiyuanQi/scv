//! Unit tests for `src/prompt/skills.rs`.

use std::collections::HashMap;

use scv_tools::builtin_registry;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::prompt::{PromptContext, SkillListings, build_system_prompt};

#[tokio::test]
async fn workspace_projects_list_their_agent_skills_for_delegation() {
    use std::os::unix::fs::symlink;
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let write_skill = |directory: &Path, description: &str| {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: skill\ndescription: {description}\n---\nBody of {description}\n"),
        )
        .unwrap();
    };
    write_skill(
        &workspace.join("scv/.agents/skills/feature-flow"),
        "Land SCV",
    );
    std::fs::create_dir_all(workspace.join("scv/.claude/skills")).unwrap();
    symlink(
        "../../.agents/skills/feature-flow",
        workspace.join("scv/.claude/skills/feature-flow"),
    )
    .unwrap();
    write_skill(
        &workspace.join("web/.claude/skills/deploy"),
        "Deploy the site",
    );
    // A linked worktree of scv: its `.git` is a file naming the checkout.
    write_skill(
        &workspace.join("scv-topic/.agents/skills/feature-flow"),
        "Land SCV from a worktree",
    );
    std::fs::write(
        workspace.join("scv-topic/.git"),
        "gitdir: ../scv/.git/worktrees/scv-topic\n",
    )
    .unwrap();
    write_skill(&workspace.join(".agents/skills/triage"), "Root triage");
    write_skill(&workspace.join(".agents/skills/notes"), "Root notes");
    write_skill(&workspace.join(".scv/skills/triage"), "SCV triage");
    write_skill(&workspace.join(".hidden/.agents/skills/secret"), "Hidden");
    write_skill(&outside.join(".agents/skills/evil"), "Outside");
    symlink(&outside, workspace.join("escape")).unwrap();
    std::fs::create_dir_all(workspace.join("rogue/.agents")).unwrap();
    symlink(
        outside.join(".agents/skills"),
        workspace.join("rogue/.agents/skills"),
    )
    .unwrap();
    std::fs::create_dir_all(workspace.join("sneaky/.agents/skills/leak")).unwrap();
    symlink(
        outside.join(".agents/skills/evil/SKILL.md"),
        workspace.join("sneaky/.agents/skills/leak/SKILL.md"),
    )
    .unwrap();
    std::fs::write(workspace.join("file"), "not a project").unwrap();
    let mut config = Config::default();
    config.skills.user_dir = workspace.join("no-user-skills");

    let skills = discover_skills(&workspace, &config, true).unwrap();
    let mut names: Vec<_> = skills.map.keys().cloned().collect();
    names.sort();
    assert_eq!(names, ["notes", "scv:feature-flow", "triage", "web:deploy"]);
    assert_eq!(
        skills.map["triage"],
        workspace.join(".scv/skills/triage/SKILL.md")
    );
    assert_eq!(
        skills.project_listing,
        "- notes (workspace root): Root notes\n\
             - scv:feature-flow (project scv): Land SCV\n\
             - web:deploy (project web): Deploy the site\n"
    );
    let listings = SkillListings {
        listing: skills.listing.clone(),
        project_listing: skills.project_listing.clone(),
    };
    let agents = ["agent_claude".to_owned(), "agent_pi".to_owned()];
    let prompt = build_system_prompt(
        &workspace,
        &config,
        &listings,
        &PromptContext {
            agents: &agents,
            background: true,
            channel: None,
        },
    )
    .unwrap();
    assert!(prompt.contains("# Project skills"));
    assert!(
        prompt.contains("such as agent_claude or agent_pi, set its cwd to the skill's project")
    );
    // Only agents this session offers are named.
    assert!(!prompt.contains("agent_codex"), "{prompt}");
    let without_agents = build_system_prompt(
        &workspace,
        &config,
        &listings,
        &PromptContext {
            agents: &[],
            background: false,
            channel: None,
        },
    )
    .unwrap();
    assert!(
        !without_agents.contains("delegate with"),
        "{without_agents}"
    );
    assert!(without_agents.contains("read_skill loads one for reference"));

    let registry = builtin_registry(
        config.tools(),
        skills.map,
        skills.roots,
        config.skills.max_skill_bytes,
        HashMap::new(),
    )
    .unwrap();
    let read_skill = registry.get("read_skill").unwrap();
    let loaded = read_skill
        .execute(
            serde_json::json!({"name":"scv:feature-flow"}),
            scv_core::ToolContext::new(workspace.clone(), CancellationToken::new()),
        )
        .await
        .unwrap();
    assert!(loaded.content.contains("Body of Land SCV"));

    let tool_free = discover_skills(&workspace, &config, false).unwrap();
    assert!(tool_free.project_listing.is_empty());
    assert!(!tool_free.map.contains_key("scv:feature-flow"));
    config.skills.scan_projects = false;
    let disabled = discover_skills(&workspace, &config, true).unwrap();
    assert!(disabled.project_listing.is_empty());
    config.skills.scan_projects = true;
    config.skills.max_skills = 3;
    let capped = discover_skills(&workspace, &config, true).unwrap();
    assert_eq!(capped.map.len(), 3);
    assert!(capped.map.contains_key("scv:feature-flow"));
    assert!(!capped.map.contains_key("web:deploy"));
}
