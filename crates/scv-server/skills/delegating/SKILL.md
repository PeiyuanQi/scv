---
name: delegating
description: How to hand work to another agent with the agent tool. Covers choosing the agent, model, and effort; writing the brief; running in the background; continuing a conversation; and what to do when a call fails. Read it before your first agent call in a session.
---

# Delegating to agents

This skill is built into SCV. A user skill named `delegating` replaces it.

## Where the facts are

Agents update themselves, and their model names and effort levels change with
each release. Nothing in this skill names a model. Read the current values
from the `agent` tool itself:

- The `agent` argument's description gives one line per agent on this
  machine. The line says what the agent offers and what it takes.
- For an agent reached over ACP, the line lists the exact `model` and
  `effort` values its server offered in its last session:
  `model (one of: …)`. Pass one of those values exactly as written.
- Where a line gives only a hint instead of a list, SCV has not seen that
  agent's list yet. Omit `model` unless the user named one. A wrong value
  then fails with the list, and SCV saves it for later sessions.
- The user's own choices are in that description and in the system prompt:
  their `use_for` note for each agent, the default model and effort SCV
  passes when a call leaves them out, the effort for a hard task, and their
  preferred order. Follow them unless the user asks for something else in
  this conversation.

Never make a model value up from a product name. "Opus 5.5" or "GPT-5.5" is
what a person says, not what an agent accepts. Find the listed value that
names that model. If exactly one matches, use it and tell the user which
value you passed. If none matches, or several do, ask the user. To refresh a
list that looks out of date, run `scv agents check <agent>` with bash. It
costs one short model turn, shows what the agent offers now, and saves it for
the next session.

## Choosing the agent

1. A `session` handle keeps its conversation's agent.
2. Next comes the agent the user named.
3. Next comes the agent whose `use_for` note matches the work.
4. Otherwise use the first of the user's preferred agents. Choose another
   only when the work needs something only that agent has, such as live web
   or X search, or when the preferred one is unavailable.

A nested SCV (`scv`) suits a self-contained side task whose details should
stay out of this conversation.

## Choosing the model and effort

- Leave `model` and `effort` out. SCV then passes the user's defaults from
  the agent's line, or the agent uses its own when the user set none.
- For a hard task, pass the hard-task effort from the agent's line as
  `effort`. A task is hard when getting it right takes sustained reasoning:
  a bug whose cause is unknown, a change that spans many files or
  components, a design, security, or data-loss decision, deep research, or
  work that already failed once at the default effort. A lookup, a summary,
  a small edit, or a known fix is not hard. When unsure, use the default.
- Otherwise pass a value only when the user asks for one, using the exact
  value from the agent's line.
- On a follow-up with `session`, leave both out unless the effort should
  change, such as when the follow-up is itself a hard task.

## Writing the brief

The agent sees nothing of this conversation, so the prompt must stand on its
own:

- **Goal:** the outcome, in one or two sentences.
- **Where:** set `cwd` to the project directory, so the agent loads that
  project's `AGENTS.md` or `CLAUDE.md` and its skills. If a project skill
  covers the work, name it, as in "land it with the feature-flow skill".
- **What you know:** file paths, error text, decisions already made, and
  links. Quote them; don't paraphrase error messages.
- **Constraints:** what not to touch, and actions that need the owner's yes
  first. Anything that cannot be undone, such as publishing, pushing to
  `main`, deleting, spending money, or messaging other people, waits for the
  owner. A delegated agent asks with `scv confirm "<question>"`.
- **Done when:** how the agent can tell it has finished, such as tests that
  pass or a file that exists.
- **Report:** what to send back, and how short.

## Running it

- Anything likely to take more than about a minute runs with `background:
  true`. Tell the user it has started only after the tool result gives you a
  job handle. If the call fails, say that it failed.
- One job per piece of work. Don't start a second attempt with a guessed
  change while the first is still running or before you have read why it
  failed.
- Follow-ups (questions, fixes, next steps) pass the result's `session`
  handle, so the agent keeps its context. Handles last only as long as this
  session, and SCV forgets a conversation left idle for a day. For a project,
  write down what matters in its notes.

## When a call fails

Read the `error`, `hint`, `fallback`, or `note` field and act on what it
says. When you tell the user what went wrong, quote the error. Don't invent a
cause.

| The result says | Do this |
| --- | --- |
| `model "…" is not one … offers; choose one of: …`, or `… is not offered by …` | Pass a listed value, found as described in *Where the facts are*. Tell the user which one you used. |
| `model "…" is the user's default for …` | Pass a listed value for this call. Tell the user their `[agents.<name>] model` is not one the agent offers, and offer the corrected line. |
| `effort "…"` is not offered | Pass a listed effort, or omit `effort`. |
| `… does not take model` (or `effort`, or `session`) | Omit that argument, or call one of the agents the error names. |
| A `hint` to run `scv agents login <agent>` | Tell the owner that exact command. Don't retry until they have signed it in. |
| A `fallback` naming other agents | That agent is missing, signed out, or its provider failed. Call one of the agents named. |
| Status `declined` | Follow the refusal rule in the system prompt. |
| Status `timeout` | Continue its `session` to ask for progress, or run it again with a longer `timeout_seconds`. |
| `conversation … ended` or `… is unknown` | Start a new conversation, with a brief that restates the context. |
| Anything else | Tell the user the error. Retry only after changing what caused it. |

## Keeping up with the agents

- `scv agents check` reports, for each agent: the version installed, whether
  it runs over ACP or through its CLI, the models and efforts it offers, and
  whether a one-line call with the user's defaults works. `scv agents status`
  shows sign-ins.
- If an agent breaks after an update, tell the owner what `scv agents check`
  says. Examples are its arguments being rejected or its output no longer
  being read. The fix is a change to SCV's adapter table, which the owner can
  ask you to hand to an agent in the `scv` project.
- When the owner states a lasting preference, such as "for coding always use
  Claude Code on Opus at xhigh" or "Grok at medium effort, high for hard
  tasks", it lasts only for this session unless it is in their
  `config.toml`. Offer the exact lines, using values from the agent's list:

  ```toml
  [agents.claude]
  use_for = "coding"
  model = "<a listed value>"
  effort = "xhigh"
  hard_task_effort = "max"   # optional: the effort for a hard task
  ```

  `model` and `effort` apply to every call to that agent that leaves them
  out, not only to the work its `use_for` names.

  Add `[agent] prefer = [...]` too if they want the order changed. Edit the
  file only after the owner says yes. New sessions read it.
