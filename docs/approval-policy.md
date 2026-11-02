# Approval and permission policy

Status: final design

The keys, their defaults, and how the old keys are read are in
[configuration.md](configuration.md#policy).

## Why

Before this policy, three separate settings decided what ran without a
person, and every default either asked or stayed out of the way:

- `[tools] approval_policy` (`on-risk` by default, `always`, `never`)
  controlled SCV's own tools and approved only read-only risk on its own.
  `never` meant *deny*, the opposite of Codex's `approval_policy = "never"`.
  No value approved everything.
- `[agents.<name>] permissions` (`default`, `full`) controlled each delegated
  CLI. `"full"` added the CLI's own switches, and SCV never checked that they
  took effect.
- `.agents/skills/feature-flow/scripts/publish.sh` asked `scv confirm` before
  every delegated publish, independent of both settings.

The owner wanted what Codex and Claude Code offer: approve by default, start
every agent in its full-autonomy mode, and always check that mode. This
document defines one policy that does that, along with the few things that
still need the owner.

## 1. One policy

```toml
[policy]
mode = "auto"                         # auto (default) | on-risk | always | read-only
publish_crate_versions_per_day = 20   # section 3

[agents.grok]
mode = "on-risk"                      # optional: this agent only
```

| `mode` | SCV's own tools, and requests relayed to SCV (ACP `session/request_permission`, nested SCV `approval.requested`) | A delegated agent starts |
| --- | --- | --- |
| `auto` | approved, including `agent` calls | in its full mode, with the existing switches |
| `on-risk` | read-only approved; the rest asks (the old default) | with no switches; its own config decides |
| `always` | every call asks | with no switches |
| `read-only` | everything but read-only denied (the old `never`) | never, because `agent` calls are delegate risk |

`[agents.<name>] mode` decides how that one agent starts and what section 2's
check expects of it. The requests it relays reach SCV as the `agent` tool's
own, so `[policy] mode` decides them.

The gates in section 4 apply on top of every mode. Strictness runs
`auto` < `on-risk` < `always` < `read-only`, and a project layer may only
move it toward `read-only`. The value `never` is not carried forward: one
owner using both Codex and SCV should not have to remember two opposite
meanings of the same word.

The decision is:

```rust
pub(crate) enum Decision { Allow, Deny, AskClient, AskOwner }
pub(crate) fn decide(policy: Policy, request: &ApprovalRequest) -> Decision
```

A matching gate (section 4) decides first, with `Deny` or `AskOwner`. After
that the mode decides: `auto` gives `Allow`, `read-only` gives `Allow` for
read-only risk and `Deny` otherwise, `on-risk` gives `Allow` for read-only
risk and `AskClient` otherwise, and `always` gives `AskClient`. `AskClient` is
the question every request that is not read-only got before: the client's
prompt, or for a background job `client_approves_all`, then the owner's
workbench, then denial.

No blanket answer can satisfy `AskOwner`. A chat bridge approves every
request in an owner session (`crates/scv-channels/src/session.rs`:
`approved = self.tools && !stale`), and background jobs inherit that approval
through `client_approves_all`. `AskOwner` skips both. In the TUI it is the
usual prompt, because the person at the terminal is the owner. Everywhere
else it goes through `Confirmer` (`crates/scv-server/src/confirm.rs`), the
path `scv confirm` uses, to the chat the work came from. No answer by the deadline counts as no. A stdio
server, which has no chat to ask in, denies it.

Where it lives, in `crates/scv-server/src/` unless a path says otherwise:

- `config/schema.rs`: `PolicyMode { Auto, OnRisk, Always, ReadOnly }`, with
  `Auto` as the default, and `PolicyConfig` for `[policy]`.
  `AdapterConfig.mode: Option<PolicyMode>` is one agent's own mode, and
  `ConfigOverrides.mode` is the `--policy` flag.
- `config/legacy.rs`: the old keys, read as their replacements (section 5).
- `config/validate.rs`: `PolicyMode::strictness()`, with `Auto` at 0. The
  publish limit is in `limits()`, so a project layer can lower it but never
  raise it.
- `config/runtime.rs`: an agent starts with its full switches when its
  effective mode is `Auto`, and a nested SCV's `args` always carry a
  `--policy` at least as strict as `on-risk` (section 2).
- `approval.rs`: `decide`, and `OwnerQuestions` for the `AskOwner` path,
  which `ProtocolApprovalGate`, `UnattendedGate`, and `PolicyFirstGate` all
  take. `confirm.rs`: `Confirmer::ask_owner`.
- The CLI flag `--policy` (`src/cli/args.rs`, `src/bin/scv-server.rs`, and
  the unit line in `src/cli/service.rs`), with `--approval-policy` as an
  alias.

## 2. Full mode by default, and a real check

Under `auto`, every delegated agent starts with the switches that
`permissions = "full"` used to add. The lists themselves did not change (see
`configuration.md`, "Policy"). Codex's takeover (`Takeover::Supported`),
offered only in its full mode, is available by default.

SCV then compares the mode the agent reports with the mode the policy
expects. This works the way `crates/scv-tools/src/delegate/output.rs` already
compares Claude's `apiKeySource` with the pinned billing identity:

- Under `auto`, the agent must report its full mode.
- Under any other mode, a report of the full mode is also a mismatch. This
  catches an agent home that turned its own prompts off. One example is an
  imported Codex `config.toml` with `approval_policy = "never"` and
  `sandbox_mode = "danger-full-access"`, which `config_notes` in
  `delegate/stores.rs` already points out.
- A run fails closed. A mismatch, or a missing report from an agent that
  should send one, stops the run: SCV kills the process or cancels the ACP
  turn, and the call fails with the expected and reported modes. A run never
  continues in a mode nobody chose.
- An agent that reports nothing still runs. Its approval summary, job record,
  and `scv agents check` say `mode unverified`.

What each agent reports (Claude Code 2.1.312, Codex 0.166.0, and the
installed `claude-agent-acp` and `codex-acp` were checked; other agents
were not):

| Agent | What reports the mode | Result |
| --- | --- | --- |
| `claude`, `claude-ai` (CLI) | The stream-json `system`/`init` event carries `permissionMode` in the same event as `apiKeySource`. A managed `permissions.disableBypassPermissionsMode` downgrades a session that asks for `bypassPermissions`, and this field shows it. | Verified before any tool runs |
| `claude`, `claude-ai` (ACP) | `session/new` lists `modes`, and the same modes as its `mode` config option. `claude-agent-acp` offers `bypassPermissions` only when bypass is allowed. It answers `session/set_mode` with `{}` even when it picks a different mode, so `select_mode` sets the `mode` config option instead, whose answer lists the mode taken. | Verified before the prompt is sent |
| `codex` (CLI) | Not in `exec --json`: `thread.started` carries only `thread_id`. The rollout file under the agent home's `sessions/` (already `conversation_files`) records a `turn_context` with `approval_policy` (`"never"`) and `sandbox_policy` (`{"type":"danger-full-access"}`). | Verified shortly after the turn starts, not before its first command. The rollout format is internal to Codex. |
| `codex` (ACP) | `modes.currentModeId` when the session opens, and the answer to setting its `mode` config option, for `agent-full-access`. `codex-acp` answers `session/set_mode` with `{}` and reports nothing. | Verified before the prompt is sent |
| `grok`, `dsh`, `copilot` | Nothing SCV reads: plain-text output, or a flag or variable with no echo | Unverified |
| `gemini`, `qwen` | Gemini's `init` event carries only `session_id` and `model` | Unverified |
| `cursor`, `amp` | A Claude-shaped `system`/`init` event. Whether it carries `permissionMode` was not confirmed, because neither CLI is installed here. | Unverified until checked against an installed CLI |
| `pi`, `opencode`, `kimi` | No switch to verify. pi asks nothing. `opencode` and `kimi` relay their prompts over ACP, and SCV's gate decides them. | Not applicable |
| `scv` (nested) | Nothing to report: the `scv` adapter's `args` are `server --stdio --policy on-risk`, so every request that is not read-only is relayed to the parent's gate. Configured `[agents.scv] args` cannot weaken that: SCV drops any `--policy` in them and appends the stricter of `on-risk` and the one dropped. | The parent's policy decides |

Over ACP, a mode counts only when the agent reports it: `modes.currentModeId`
(else the `mode` config option's `currentValue`) when the session opens, a
`session/set_config_option` answer listing the `mode` option (including
answers that set the model or effort), a
`current_mode_update`, or a `config_option_update` listing the `mode` option.
An answer of `{}` is not a report, so selecting the full mode without one
fails the check like a mismatch. SCV records the mode for any agent that
reports one. For agents without `full_mode` (grok, dsh, gemini, qwen), SCV
has no expected value to compare it with, so they stay unverified. After the
check passes, a later report of another mode fails the call. Claude's own
`plan` mode is the one exception, because it returns to the mode it left.

Where it lives, in `crates/scv-tools/src/`:

- `delegate/adapters.rs`: `AdapterDescriptor`'s
  `mode_report: ModeReport { ClaudeInit, CodexRollout, None }`. ACP agents with
  `AcpLaunch.full_mode` are checked through their session instead.
- `delegate/mode.rs`: what the policy expects of a reported mode, the Codex
  rollout reader, and the summary's and the result's words for the mode.
- `delegate/output.rs`: reads `permissionMode` next to `apiKeySource`. A
  mismatch takes the same stop path as a credential mismatch.
- `delegate/acp/session.rs`: `select_mode` returns the mode the agent
  reported during the call, or none, and the session watches the mode
  reports for its whole life.
- `delegate/native.rs`: reads the Codex rollout after `thread.started`, for up
  to 30 seconds and once more when the run ends.
- `delegate/native.rs` and `delegate/acp/tool.rs`: the `FULL PERMISSIONS`
  summary names the policy and whether the mode is verified. A result's
  `mode` and a job record's `mode` say `verified <mode>` or `unverified`.
- `scv agents check` shows each agent's reported mode.

## 3. Publishing

The one publish rule: publishing a crate to crates.io needs no question
unless it would take that crate past `publish_crate_versions_per_day` (20)
versions in the last 24 hours. In that case SCV asks the owner, and no answer
within 30 minutes
(`SCV_CONFIRM_TIMEOUT`) counts as no. A crate already at crates.io's own
limit stops the release without a question. Publishing anywhere else still
waits for a yes (section 4).

- **Counting.** crates.io's limit is 20 new versions per crate over a rolling
  24 hours, not per calendar day, and SCV counts the same way. For each crate
  about to be published, it counts the versions crates.io lists
  (`GET /api/v1/crates/<crate>/versions`, yanked versions included) whose
  `created_at` falls in the last 24 hours. An SCV release publishes each of
  its nine crates once, so it counts one version against each crate. The
  limit therefore allows 20 releases in any 24 hours, not 20 `cargo publish`
  runs. A count that cannot be fetched counts as over the line.
- **Whole release first.** Every pending crate is checked before the first
  `cargo publish`. A release that would cross the line partway therefore
  never starts, so it cannot be left half-published.
- **At 20.** crates.io refuses a crate's 21st version in 24 hours whatever
  the owner answers, after the crates before it in the release were
  published. So a crate with 20 versions in the last 24 hours blocks the
  release without a question, whatever `publish_crate_versions_per_day`
  says: the check names the crates at the limit and exits 1, and nothing is
  published. A number above 20 therefore acts as 20, and a lower number is
  the only line where the owner's yes can publish.

Where it lives:

- `src/cli/policy.rs`: `scv policy check publish --version <v> <crate>...`.
  It loads `Config::load` the way `src/cli/update.rs` does and counts through
  crates.io with the root package's `reqwest`. It decides with the rule,
  which lives in `crates/scv-server/src/publish.rs` (policy authority stays
  in `scv-server`, as `AGENTS.md` requires). Over the line, it asks with
  `DaemonCommand::ConfirmAsk`, as `src/cli/confirm.rs` does. Its exit codes
  match `scv confirm`: 0 to go, 1 for no (a release blocked at crates.io's
  limit included), 2 when it could not decide or ask.
- `publish.sh` makes one call,
  `"$here/host.sh" scv policy check publish --version "$version" "${pending[@]}"`,
  before its first `cargo publish`. It runs from a terminal too, where a
  question goes to the `[notify]` owner chat. When the installed `scv` has no
  `policy` subcommand, the script falls back to asking `scv confirm` before
  every publish by an agent SCV delegated to, as it did before the rule, so
  the first release that carries the rule is not published unasked.

## 4. What stays gated under `auto`

The rules come from the workspace `AGENTS.md` (`~/projects/AGENTS.md`,
"Approval for actions that cannot be undone") and `feature-flow`. This
policy changed one line of them, publishing within the limit, which was the
owner's decision. Nothing else widened.

**Never allowed, and never asked** (AGENTS.md: "Never skip a repository's
required CI or review, force-push `main`, or stash, reset, or discard work
that is not the task's"; feature-flow stage 5):

- force-pushing `main`. The `--force-with-lease` on the task's own branch in
  feature-flow stage 5 stays allowed.
- skipping a repository's required CI or review.
- stashing, resetting, or discarding work that is not the task's.

**Needs the owner's yes** (`AskOwner`, where no answer is no):

- deleting anything that is not the task's own: other branches or worktrees,
  data, or uncommitted work. A task's own branch and worktree may still be
  removed once its change is confirmed on the target branch, after the owner
  asked to land.
- pushing to a shared branch. The exception is landing that the owner
  explicitly asked for, which is AGENTS.md's one exception for that task's
  change after its checks, CI, and review.
- installing, deploying, or restarting without an explicit request for it.
  A request to land never implies one.
- spending money: buying, subscribing to, or provisioning anything paid.
  Model and agent usage under the configured accounts does not count.
- messaging anyone but the owner. Mail actions keep their own approval
  (`mail.actions`, `channels.md`) unchanged.
- publishing anywhere but crates.io, or to crates.io over the line
  (section 3).
- a request whose repository, change, or target branch is unclear, or work
  that has grown beyond what the owner asked for.

Auto-approve removes per-action questions. It does not authorize work nobody
asked for: feature-flow's "Run stages 5-8 only when the user asked to land,
ship, or release" still holds.

How each layer enforces this:

| Layer | What it sees | What it enforces |
| --- | --- | --- |
| SCV's gate (`decide`) | SCV's own tool calls, and requests relayed to it | Built-in command gates: `git push` that forces `main` gets `Deny`, and a direct `cargo publish` gets `AskOwner` (the counted path is `publish.sh`). These match the exact command, which `ApprovalRequest` (`crates/scv-core/src/approval.rs`) gains as a field, because `summary` is display text. |
| The agent's own deny rules | Its own tool calls, even in full mode | Claude Code only. Its `bypassPermissions` "auto-approves every tool call (except explicit deny rules)" (2.1.312). SCV passes `--disallowedTools` with the force-push-to-`main` spellings its prefix patterns can express and `Bash(cargo publish:*)`; over ACP it passes them as `_meta.claudeCode.options.disallowedTools`. Codex 0.166.0 has `prefix_rule` rules files, but nothing confirms that a forbidden rule holds under `--dangerously-bypass-approvals-and-sandbox`, so nothing relies on it. |
| Scripts | What they run | `publish.sh` calls `scv policy check publish` |
| Instructions | Nothing | AGENTS.md, feature-flow, and the delegating brief's constraints: the agent asks with `scv confirm` |

In full mode, a delegated agent relays nothing, so SCV sees none of its
commands. For every agent except Claude, each gate above is an instruction
the agent is trusted to follow, apart from `publish.sh` and crates.io's own
limit. Command matching catches the plain spelling, not a script or
`bash -c` that hides it. It is a guardrail against mistakes, not a security
boundary; `security.md` already says SCV is not one.

## 5. Migration

The old keys stay accepted as deprecated aliases, each read with its meaning
kept: `[tools] approval_policy` as `[policy] mode` (`never` as `read-only`),
and `[agents.<name>] permissions` as that agent's `mode` (`"full"` as
`auto`, `"default"` as `on-risk`, so that agent keeps starting with no
switches). With neither key set, the mode is `auto`. The rules, the startup
warnings, and what `scv config show` marks are in
[configuration.md](configuration.md#migrating-from-approval_policy-and-permissions).

The workspace `AGENTS.md` (the owner's file, outside this repository),
`feature-flow`, and the delegating skill state the same rules: a release the
owner asked for publishes without a further yes within the daily limit,
`feature-flow`'s publish script asks past it, and section 4's gates still
wait for the owner.

## Risks

With `auto` as the default, a delegated agent can push and publish without a
person in the loop. Three things bound that:

1. **The publish limit.** It is checked for the whole release before the
   first `cargo publish`. crates.io's own limit backs it up and holds even
   against an agent that skips the script. Even so, a runaway agent can
   publish up to 20 versions of each crate in 24 hours, and versions can be
   yanked but never deleted.
2. **The still-gated actions.** Force-pushing `main` is denied by SCV's gate
   and by Claude's deny rules. Everything else in section 4 needs an explicit
   yes that no blanket answer can give.
3. **The mode check, which fails closed.** No run continues in a mode nobody
   chose. That includes the dangerous direction: an agent home that turned
   prompts off while the policy said to ask.

These bounds leave real gaps:

- SCV cannot see a full-mode agent's commands. For agents other than Claude,
  the gates are instructions. An agent that ignores them can force-push or
  run `cargo publish` directly with the owner's real home (`host.sh`). The
  hard stops are then outside SCV: crates.io's limit, and GitHub branch
  protection, which SCV does not configure.
- `auto` removes the TUI's per-call prompt, which can catch a command
  injected by a fetched page, a file, or an issue. Owner chat sessions
  approved everything before; `auto` extends that to the terminal and to the
  agents' own CLIs. Reading secret-like files and `web_fetch` to any host are
  both allowed, and together they can send a secret out.
  `mode = "on-risk"` brings the per-call prompt back.
- grok, dsh, gemini, qwen, copilot, cursor, and amp run unverified. A
  stricter mode for one of them is a request SCV cannot confirm.
- The Codex check reads an internal file and runs after the turn starts. A
  Codex update that changes the format makes every Codex run fail closed
  until the adapter is updated. That failure is intended, but it is still an
  outage.
