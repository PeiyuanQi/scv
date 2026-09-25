//! Finding skills: SCV's own and the user's, which `read_skill` serves, and
//! the agent skills of the workspace's projects, which delegated agents load.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use scv_tools::SkillMap;

use super::read_prefix;
use crate::config::Config;

/// Skills found at session start: the names `read_skill` serves, the roots it
/// revalidates them against, and their system-prompt listings.
pub(crate) struct DiscoveredSkills {
    pub(crate) map: SkillMap,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) listing: String,
    pub(crate) project_listing: String,
}

/// Agent-native skill directories, relative to a project, that Codex and
/// Claude Code load from their working directory.
pub(crate) const PROJECT_SKILL_DIRS: [&str; 2] = [".agents/skills", ".claude/skills"];
/// Workspace entries and child projects inspected for project skills, so a
/// large workspace such as a home directory costs bounded lookups.
pub(crate) const MAX_WORKSPACE_ENTRIES: usize = 4096;
pub(crate) const MAX_SKILL_PROJECTS: usize = 256;
/// Bytes read to find a project skill's description, and its listed length.
pub(crate) const PROJECT_SKILL_HEADER_BYTES: usize = 16 * 1024;
pub(crate) const MAX_PROJECT_SKILL_DESCRIPTION: usize = 400;

pub(crate) fn discover_skills(
    workspace: &Path,
    config: &Config,
    tools: bool,
) -> Result<DiscoveredSkills> {
    let mut skills = SkillMap::new();
    let mut roots = Vec::new();
    let project_root = workspace.join(&config.skills.project_dir);
    for (root, must_be_workspace) in [(&project_root, true), (&config.skills.user_dir, false)] {
        if !root.is_dir() {
            continue;
        }
        let canonical = std::fs::canonicalize(root)
            .with_context(|| format!("resolve skill root {}", root.display()))?;
        if must_be_workspace && !canonical.starts_with(workspace) {
            return Err(anyhow!("project skill root escaped workspace"));
        }
        roots.push(canonical.clone());
        let mut entries: Vec<_> = std::fs::read_dir(&canonical)
            .with_context(|| format!("read skill root {}", canonical.display()))?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            if skills.len() >= config.skills.max_skills {
                break;
            }
            let path = entry.path().join("SKILL.md");
            if !path.is_file() {
                continue;
            }
            let canonical_file = std::fs::canonicalize(&path)
                .with_context(|| format!("resolve skill {}", path.display()))?;
            if !canonical_file.starts_with(&canonical) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            skills.entry(name).or_insert(canonical_file);
        }
    }
    let mut names: Vec<_> = skills.keys().cloned().collect();
    names.sort();
    let mut listing = String::new();
    for name in names {
        let path = &skills[&name];
        let bytes = read_prefix(path, config.skills.max_skill_bytes)
            .map(|(bytes, _)| bytes)
            .unwrap_or_default();
        let content = String::from_utf8_lossy(&bytes);
        let description = skill_description(&content);
        listing.push_str(&format!("- {name}: {description}\n"));
    }
    // Project skills are only actionable by delegating, so tool-free sessions
    // neither list them nor learn the workspace's project names.
    let project_listing = if tools && config.skills.scan_projects {
        discover_project_skills(workspace, config, &mut skills, &mut roots)
    } else {
        String::new()
    };
    Ok(DiscoveredSkills {
        map: skills,
        roots,
        listing,
        project_listing,
    })
}

/// List the agent skills of the workspace and its immediate, non-hidden child
/// projects. A child's skills are named `<project>:<skill>`. Everything
/// resolves inside the workspace, SKILL.md files that resolve to the same file
/// (such as a `.claude/skills` link to `.agents/skills`) count once, and
/// unreadable entries are skipped so one broken project cannot stop a session.
pub(crate) fn discover_project_skills(
    workspace: &Path,
    config: &Config,
    skills: &mut SkillMap,
    roots: &mut Vec<PathBuf>,
) -> String {
    let mut projects = vec![(None, workspace.to_path_buf())];
    let mut names: Vec<_> = std::fs::read_dir(workspace)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .take(MAX_WORKSPACE_ENTRIES)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    for name in names {
        if projects.len() > MAX_SKILL_PROJECTS {
            break;
        }
        let Ok(directory) = std::fs::canonicalize(workspace.join(&name)) else {
            continue;
        };
        // A linked git worktree (its `.git` is a file) is another checkout of
        // a project already listed; its skills would appear twice.
        if directory.is_dir()
            && directory.starts_with(workspace)
            && !directory.join(".git").is_file()
            && !projects.iter().any(|(_, seen)| seen == &directory)
        {
            projects.push((Some(name), directory));
        }
    }
    let mut seen_files = std::collections::HashSet::new();
    let mut listing = String::new();
    'projects: for (project, directory) in projects {
        let project_roots: Vec<PathBuf> = PROJECT_SKILL_DIRS
            .iter()
            .filter_map(|relative| std::fs::canonicalize(directory.join(relative)).ok())
            .filter(|root| root.is_dir() && root.starts_with(workspace))
            .collect();
        for root in &project_roots {
            if !roots.contains(root) {
                roots.push(root.clone());
            }
        }
        for root in &project_roots {
            let mut entries: Vec<_> = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .take(MAX_WORKSPACE_ENTRIES)
                .collect();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                if skills.len() >= config.skills.max_skills {
                    break 'projects;
                }
                let Ok(file) = std::fs::canonicalize(entry.path().join("SKILL.md")) else {
                    continue;
                };
                if !file.is_file()
                    || !project_roots.iter().any(|root| file.starts_with(root))
                    || !seen_files.insert(file.clone())
                {
                    continue;
                }
                let skill = entry.file_name().to_string_lossy().into_owned();
                let (name, location) = match &project {
                    Some(project) => (format!("{project}:{skill}"), format!("project {project}")),
                    None => (skill, "workspace root".to_owned()),
                };
                // SCV's own and the user's skills keep their names.
                if skills.contains_key(&name) {
                    continue;
                }
                let header = read_prefix(
                    &file,
                    config
                        .skills
                        .max_skill_bytes
                        .min(PROJECT_SKILL_HEADER_BYTES),
                )
                .map(|(bytes, _)| bytes)
                .unwrap_or_default();
                let description: String = skill_description(&String::from_utf8_lossy(&header))
                    .chars()
                    .take(MAX_PROJECT_SKILL_DESCRIPTION)
                    .collect();
                listing.push_str(&format!("- {name} ({location}): {description}\n"));
                skills.insert(name, file);
            }
        }
    }
    listing
}

pub(crate) fn skill_description(content: &str) -> String {
    if let Some(frontmatter) = content.strip_prefix("---\n")
        && let Some((header, _)) = frontmatter.split_once("\n---")
    {
        for line in header.lines() {
            if let Some(description) = line.strip_prefix("description:") {
                return description.trim().trim_matches('"').to_owned();
            }
        }
    }
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("No description provided")
        .chars()
        .take(240)
        .collect()
}

#[cfg(test)]
mod tests;
