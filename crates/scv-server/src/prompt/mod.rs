//! The session's system prompt: the configured base, the model's reasoning
//! effort, project instructions, skills, how to delegate, and the chat
//! channel it answers on.

pub(crate) mod skills;

use std::{io::Read as _, path::Path};

use anyhow::{Context, Result, anyhow};

use crate::config::Config;

/// The skill listings a session's system prompt carries.
pub(crate) struct SkillListings {
    pub(crate) listing: String,
    /// Built-in skills, listed only when the session offers agents.
    pub(crate) builtin_listing: String,
    pub(crate) project_listing: String,
}

/// What the system prompt tells the model about its situation.
pub(crate) struct PromptContext<'a> {
    /// Agents this session's `agent` tool offers, such as `codex`, sorted.
    pub(crate) agents: &'a [String],
    /// Whether agent calls can run in the background.
    pub(crate) background: bool,
    /// Whether agent calls can run as reviewed jobs (`review`).
    pub(crate) review: bool,
    /// The chat channel the session answers on.
    pub(crate) channel: Option<&'a str>,
    /// Whether the model can look through the chat's log (`chat_history`)
    /// and keep files from it (`chat_keep`).
    pub(crate) chat_history: bool,
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
    if let Some(effort) = &config.provider.reasoning_effort {
        // The agent tool takes an effort too; keep the model from mistaking
        // a delegated agent's setting for its own.
        prompt.push_str(&format!(
            "You run on model {} at reasoning effort {effort}, as SCV's provider \
             configuration sets it.",
            config.provider.model
        ));
        if !context.agents.is_empty() {
            prompt.push_str(" The effort you pass to the agent tool sets only that agent's.");
        }
        prompt.push('\n');
    }
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
    let mut listing = skills.listing.clone();
    if !context.agents.is_empty() {
        listing.push_str(&skills.builtin_listing);
    }
    if !listing.is_empty() {
        prompt.push_str("\n# Available skills\n");
        prompt.push_str(&listing);
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
                "\nTo use one, call the agent tool with an agent such as {}, set its cwd to \
                 the skill's project, and name the skill in the prompt: that agent then loads \
                 the project's instructions and skills itself. read_skill loads a skill for \
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
        if context.chat_history {
            prompt.push_str(
                "Earlier conversations in this chat are kept in a log that outlasts this \
                 session. When the user refers to something that is not in this conversation, \
                 look it up with chat_history instead of guessing or asking them to repeat it. \
                 Files the user sends are removed after a while; when they ask to keep one, \
                 use chat_keep.\n",
            );
        }
    }
    Ok(prompt)
}

/// How the main agent works with delegated agents. Written to explain why,
/// since the model follows guidance it understands more reliably.
pub(crate) fn delegation_guidance(config: &Config, context: &PromptContext<'_>) -> String {
    let named: Vec<String> = context
        .agents
        .iter()
        .map(|agent| format!("{agent} ({})", scv_tools::agent_choice::product(agent)))
        .collect();
    let mut text = format!(
        "\n# Delegating work\n\
         You can hand work to other agents with the agent tool, naming one in its agent \
         argument: {}. That argument's description says what each agent offers and which \
         options it takes, with the exact model and effort values an agent listed. Read \
         the delegating skill before your first agent call in a session: it covers \
         choosing the agent, model, and effort, writing the brief, and what to do when a \
         call fails.",
        named.join(", ")
    );
    let preferred: Vec<&str> = config
        .agent
        .prefer
        .iter()
        .map(String::as_str)
        .filter(|agent| context.agents.iter().any(|offered| offered == agent))
        .collect();
    if let Some(first) = preferred.first() {
        text.push_str(&format!(
            " The user prefers {}, in that order; choose another when the work needs \
             something only it offers, or when a preferred one is unavailable. A call that \
             names no agent goes to {first}, so name one whenever the work calls for another.",
            preferred.join(", ")
        ));
    }
    text.push_str(&task_defaults_guidance(config, context));
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
        if context.review {
            text.push_str(
                "\nAn agent call may set review to run the job with an independent reviewer: a \
                 fresh agent checks the work and returns a structured verdict, and the builder \
                 fixes blocking findings, for a bounded number of rounds. Set review only when \
                 the user asks for a review or accepts your suggestion of one, never on your \
                 own. Suggest it once per task for coding work with a checkable outcome that \
                 will be landed, merged, released, or deployed, that touches security, \
                 credentials, data deletion or migration, or concurrency, that spans several \
                 files or components, or that already went wrong once; not for lookups, \
                 research, summaries, small or known fixes, or status checks, and not where \
                 the project's instructions or notes say not to. When the task itself lands, \
                 publishes, or deploys, ask before starting, because the review must come \
                 first; otherwise start the job as usual and offer a review in one line. SCV \
                 states the outcome in its own Review and Landing lines: only an approved \
                 review is approved, and landed work is not approved work. Never start another \
                 reviewed call to get past an unresolved or escalated review unless the user \
                 asks. The delegating skill's Review section has the details.\n",
            );
        }
    } else {
        text.push_str(
            "\n\nHand substantial work to an agent rather than doing it step by step with \
             bash. The agent does not see this conversation, so write a brief that stands on \
             its own: the goal, the project directory (cwd), what you already know, \
             constraints, and what to report back.\n",
        );
    }
    if context.agents.iter().any(|agent| agent == "grok") {
        text.push_str(
            "\nIf an agent declines a request, including a safety or guardrail refusal, tell \
             the user what it said and call the agent tool with agent grok and the same \
             request. A refusal from another agent is not a reason to skip Grok. If grok \
             itself declines, tell the user; don't pass the request to another agent on your \
             own unless the user names one.\n",
        );
    } else {
        // No Grok in this session: a refusal goes back to the user, who may
        // still name another agent, whose policies then apply.
        text.push_str(
            "\nIf an agent declines a request, tell the user what it said; don't pass the \
             request to another agent on your own. If the user then asks for a specific \
             agent, use it.\n",
        );
    }
    text
}

/// Per-agent `use_for` notes, the defaults SCV passes when a call leaves out
/// model or effort, and the effort for a hard task, as one calm paragraph.
fn task_defaults_guidance(config: &Config, context: &PromptContext<'_>) -> String {
    use scv_tools::agent_choice::{defaults_phrase, note_clause};

    let mut sentences = Vec::new();
    let mut defaults = Vec::new();
    let mut hard = Vec::new();
    for agent in context.agents {
        let Some(adapter) = config.agents.0.get(agent) else {
            continue;
        };
        if let Some(use_for) = adapter.use_for.as_deref() {
            sentences.push(capitalize_ascii(&format!(
                "for {}, prefer {agent}",
                note_clause(use_for)
            )));
        }
        let configured = adapter.defaults();
        if let Some(phrase) = defaults_phrase(&configured) {
            defaults.push(format!("{phrase} to {agent}"));
        }
        if let Some(effort) = &configured.hard_task_effort {
            hard.push(format!("effort {effort} to {agent}"));
        }
    }
    if !defaults.is_empty() {
        sentences.push(format!(
            "When a call leaves out model or effort, SCV passes the user's defaults: {}",
            joined(&defaults)
        ));
    }
    if !hard.is_empty() {
        sentences.push(format!("For a hard task, pass {}", joined(&hard)));
    }
    sentences
        .iter()
        .map(|sentence| format!(" {sentence}."))
        .collect()
}

/// `a`, `a, and b`, or `a, b, and c`: the comma keeps items that contain
/// "and" apart.
fn joined(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn capitalize_ascii(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
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
