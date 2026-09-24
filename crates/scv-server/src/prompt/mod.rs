//! The session's system prompt: the configured base, project instructions,
//! skills, how to delegate, and the chat channel it answers on.

pub(crate) mod skills;

use std::{io::Read as _, path::Path};

use anyhow::{Context, Result, anyhow};

use crate::config::Config;

/// The skill listings a session's system prompt carries.
pub(crate) struct SkillListings {
    pub(crate) listing: String,
    pub(crate) project_listing: String,
}

/// What the system prompt tells the model about its situation.
pub(crate) struct PromptContext<'a> {
    /// Agent tools this session offers, such as `agent_codex`, sorted.
    pub(crate) agents: &'a [String],
    /// Whether agent calls can run in the background.
    pub(crate) background: bool,
    /// The chat channel the session answers on.
    pub(crate) channel: Option<&'a str>,
}

pub(crate) fn build_system_prompt(
    workspace: &Path,
    config: &Config,
    skills: &SkillListings,
    context: &PromptContext<'_>,
) -> Result<String> {
    let mut prompt = config.agent.system_prompt.clone();
    prompt.push_str(&format!(
        "\nCurrent working directory: {}\n",
        workspace.display()
    ));
    let agents_path = workspace.join("AGENTS.md");
    if agents_path.is_file() {
        let canonical = std::fs::canonicalize(&agents_path).context("resolve project AGENTS.md")?;
        if !canonical.starts_with(workspace) {
            return Err(anyhow!("project AGENTS.md escaped workspace"));
        }
        let (bytes, truncated) = read_prefix(&canonical, config.tools.max_read_bytes)
            .context("read project AGENTS.md")?;
        let instructions = std::str::from_utf8(&bytes).context("project AGENTS.md is not UTF-8")?;
        prompt.push_str("\n# Project instructions\n");
        prompt.push_str(instructions);
        if truncated {
            prompt.push_str("\n[AGENTS.md truncated by configured read limit]\n");
        }
    }
    if !skills.listing.is_empty() {
        prompt.push_str("\n# Available skills\n");
        prompt.push_str(&skills.listing);
        prompt.push_str("\nUse read_skill with a skill name when its workflow applies.\n");
    }
    if !skills.project_listing.is_empty() {
        prompt.push_str("\n# Project skills\n");
        prompt.push_str(
            "Projects in this workspace provide these skills to agents working in them:\n",
        );
        prompt.push_str(&skills.project_listing);
        match context.agents {
            [] => prompt.push_str("\nread_skill loads one for reference.\n"),
            agents => prompt.push_str(&format!(
                "\nTo use one, delegate with an agent tool such as {}, set its cwd to the \
                 skill's project, and name the skill in the prompt: that agent then loads the \
                 project's instructions and skills itself. read_skill loads a skill for \
                 reference.\n",
                agents
                    .iter()
                    .take(2)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" or ")
            )),
        }
    }
    if !context.agents.is_empty() {
        prompt.push_str(&delegation_guidance(config, context));
    }
    if let Some(channel) = context.channel {
        prompt.push_str(&format!(
            "\n# Chat channel\n\
             This conversation takes place on {channel}. The user reads your replies there as \
             chat messages, so keep them short and in plain text, without tables, headings, \
             or code blocks unless the user asks for them. Only the last message of each turn \
             reaches the user, and they never see your tool calls or their output, so put what \
             you did and what you found into that message in words.\n"
        ));
    }
    Ok(prompt)
}

/// How the main agent works with delegated agents. Written to explain why,
/// since the model follows guidance it understands more reliably.
pub(crate) fn delegation_guidance(config: &Config, context: &PromptContext<'_>) -> String {
    let named: Vec<String> = context
        .agents
        .iter()
        .map(|tool| format!("{tool} ({})", scv_tools::agent_choice::product(tool)))
        .collect();
    let mut text = format!(
        "\n# Delegating work\n\
         You can hand work to these agents: {}. Each tool's description says what that agent \
         offers.",
        named.join(", ")
    );
    let preferred: Vec<String> = config
        .agent
        .prefer
        .iter()
        .map(|agent| format!("agent_{agent}"))
        .filter(|tool| context.agents.contains(tool))
        .collect();
    if !preferred.is_empty() {
        text.push_str(&format!(
            " The user prefers {}, in that order; choose another when the work needs \
             something only it offers, or when a preferred one is unavailable.",
            preferred.join(", ")
        ));
    }
    if context.background {
        text.push_str(
            "\n\nStay available to the user: while one of your turns runs, they cannot reach \
             you. Handle quick things yourself, such as short reads, lookups, status checks, \
             and answers you can give in a step or two. Hand real work to an agent with \
             background set to true: changes to code or files, multi-step investigation, \
             builds, tests, releases, and anything else likely to take more than about a \
             minute. Then reply right away with what you started and its job handle.\n\n\
             The agent does not see this conversation, so write a brief that stands on its \
             own: the goal, the project directory (cwd), what you already know, constraints, \
             and what to report back.\n\n\
             When a job finishes, SCV starts a turn with an [SCV background report]; tell the \
             user what happened and the key result. agent_status shows how jobs are going, \
             and agent_cancel stops one the user no longer wants. agent_wait, foreground \
             agent calls, and long bash commands keep the user waiting, so use them only for \
             results you need within this turn that arrive quickly.\n",
        );
    } else {
        text.push_str(
            "\n\nHand substantial work to an agent rather than doing it step by step with \
             bash. The agent does not see this conversation, so write a brief that stands on \
             its own: the goal, the project directory (cwd), what you already know, \
             constraints, and what to report back.\n",
        );
    }
    // A refusal is the agent's own judgement, so it goes back to the user;
    // the user may still choose another agent, whose policies then apply.
    text.push_str(
        "\nIf an agent declines a request, tell the user what it said; don't pass the request \
         to another agent on your own. If the user then asks for a specific agent, use it.\n",
    );
    text
}

pub(crate) fn read_prefix(path: &Path, max_bytes: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(max_bytes.min(8192));
    file.take(
        u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    )
    .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    Ok((bytes, truncated))
}

#[cfg(test)]
mod tests;
