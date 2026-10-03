# Release and Compatibility

Status: final design

The current workspace release is `0.3.9`. All crates share that version, and
dependencies between workspace packages use exact `=0.3.9` pins.

SCV supports the latest patch release of stable Rust 1.88 or newer on:

- macOS 13 or newer on Apple Silicon and x86-64;
- glibc-based Linux on x86-64 and ARM64.

The release workflow builds and tests four target archives:

- `scv-aarch64-apple-darwin.tar.gz`;
- `scv-x86_64-apple-darwin.tar.gz`;
- `scv-aarch64-unknown-linux-gnu.tar.gz`;
- `scv-x86_64-unknown-linux-gnu.tar.gz`.

Each archive contains `scv`, `scv-server`, `README.md`, `LICENSE`, and
`NOTICE`. Checksums are published beside the archives. Release builds use Cargo
locked mode. The project does not ship a curl-to-shell installer.

SCV is licensed under the Apache License 2.0. The root `LICENSE` contains the
unmodified Apache 2.0 license text, `NOTICE` identifies SCV and any required
third-party notices, and the workspace and every published Cargo package set
`license = "Apache-2.0"`. Dependency license checks reject packages whose terms
are incompatible with Apache-2.0 distribution.

The root `README.md` is the installation and quick-start contract. It includes
prerequisites, provider configuration, source build, binary usage, safety
limits, extension entry points, development checks, architecture links,
contribution guidance, and license information.

Installed clients can update with `scv update`. The command uses crates.io by
default, accepts a Cargo index override through `[update].index_url`,
`SCV_CARGO_INDEX_URL`, or `--index-url`, and installs the published `scv-cli`
binary through Cargo. It restarts an active systemd user daemon after
installation. A foreground `scv run` daemon requires an explicit restart.
Existing TUI clients reconnect and start fresh sessions without restoring
server history or automatically replaying submitted or queued work.

Multiple SCV profiles may run concurrently. Select one with `--scv-home` or
`SCV_HOME`; each profile has an independent socket, systemd user unit, provider
configuration, model selection, credentials, channel state, and nested-agent
state. Custom profile selectors are persisted by `scv start`/`restart`, and
`scv update` restarts only the selected daemon.

Before enabling supervised WeChat accounts, stop any `0.1.9` standalone bridge
processes manually: they do not honor the new account locks. Legacy credentials
and unbound delivery state are loaded conservatively; changing an account's
identity or API origin requires explicit logout before login. See
[channel identity and durable state](channels.md#identity-and-durable-state).

## Upgrading to 0.3.9

`0.3.9` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3,
so a planned restart from `0.3.8` checks the new release and can roll it back.

- **Slack.** A new chat channel, `slack`, through a bot app the owner creates
  from `slack-app.example.json` with Socket Mode on, signed in with
  `scv channels login slack` and its bot and app-level tokens entered by
  hand. It answers like Feishu: owner-only by default, files both ways,
  shared messages and threads' roots as context, threads as conversations of
  their own, and catch-up from conversation history after a reconnect.
  Nothing of it runs until an account signs in, and Feishu stays the
  recommended channel. See [Slack contract](channels.md#slack-contract).
- `0.3.8` rejects a `[channels.slack.<account>]` table in `config.toml`, which
  `scv channels run slack` writes, so it cannot start with one, and an
  automatic rollback would fail too. Remove those tables before going back to
  `0.3.8`; it ignores Slack's saved credentials and state.

What changes for code that embeds SCV's crates: `scv-channels` gains the
`slack` feature, on by default, with `ChannelKind::Slack` and
`ChannelCredentials::Slack`, so an exhaustive `match` on either adds an arm.

## Upgrading to 0.3.8

`0.3.8` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3,
so a planned restart from `0.3.7` checks the new release and can roll it back.

- **Feishu threads.** A message in a Feishu thread (话题) is answered inside
  the thread, on a daemon session of its own with its chat's authority; the
  owner's threads in their direct chat are logged as conversations of their
  own, and their background reports go into the thread. Questions and
  notices still go to the direct chat itself. See
  [Feishu threads](channels.md#feishu-threads).
- **DeepSeek Harness takes `model` and `effort` over ACP.** Its ACP server
  lists every configured provider's models as `["provider","model"]` and its
  reasoning efforts; SCV now lists and takes them (as `provider/model`), so
  `[agents.dsh] model`, `effort`, and `hard_task_effort` apply to it. Run once
  per turn it still takes neither. ACP option values listed in groups are
  read too. See [Model and effort values](tools.md#model-and-effort-values).
- **SCV's own reasoning effort.** A provider profile's `reasoning_effort`,
  such as `"high"`, goes with every request as the Responses
  `reasoning.effort`, and the system prompt states it. Without it requests
  are unchanged. See [Reasoning effort](configuration.md#reasoning-effort).
- **State stays readable both ways.** The Feishu checkpoint gains per-thread
  times and a running job its thread, both of which `0.3.7` ignores. After
  going back to `0.3.7`, a reply still waiting to go into a thread is refused
  by Feishu and held, and a thread's message is answered in its chat.
- `0.3.7` rejects `reasoning_effort` in a provider table, so with one in
  `config.toml` it cannot start, and an automatic rollback would fail too.
  Remove it before going back to `0.3.7`.

What changes for code that embeds SCV's crates:

- `scv_tools::AcpAgentLaunch` gains `session_options` (the ACP server takes
  `model` and `effort` as session options where the CLI takes neither), and
  `scv_tools::adapters::AcpLaunch` the same field, so code that builds an
  `AcpAgentLaunch` with a struct literal sets it: `false` keeps the earlier
  behavior.
- `OpenAiProvider::with_reasoning_effort` asks for an effort on every
  request, and `ProviderConfig` carries `reasoning_effort`.

## Upgrading to 0.3.7

`0.3.7` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3.
Process liveness checks identify descendants by PID and start time and treat
zombies as stopped on Linux and macOS. Regression tests isolate their caller's
delegation context explicitly, with production-path coverage of inherited
depth and parent-chain propagation.

## Upgrading to 0.3.6

`0.3.6` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3.
Delegated native, ACP, and nested-SCV coding agents now resolve an installed
Rust toolchain for the selected project before launch, while keeping Cargo and
agent state private. `scv agents doctor` exposes the same offline preflight
diagnostics without starting a daemon or model call. See [Project Rust
environment](tools.md#project-rust-environment).

## Upgrading to 0.3.5

`0.3.5` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3,
so a planned restart from `0.3.4` checks the new release and can roll it back,
as long as the instance does not use mail yet (below).

What changes for a person running SCV:

- **Read-only mail triage.** `scv channels login email` signs an IMAP mailbox
  in (a password, or the authorization code QQ, Foxmail, and 163 require),
  and `[channels.email.<account>.mail]` says what to watch, how much a model
  may read and spend, and which mail chats to report to. The account decides
  each new message with rules first and a tool-free model turn only within
  its budget, and sends deterministic digests. It cannot write to a mailbox or
  send mail. See [Email](channels.md#email) and
  [Mail accounts](configuration.md#mail-accounts).
- **Mail chats.** A chat account with `purpose = "mail"` (`scv channels run
  <channel> --account NAME --purpose mail`) carries only mail reports: no
  model answers in it, it is never chat-logged, never takes SCV's notices or
  questions, and once it has carried mail it refuses to run as an ordinary
  chat until logout. See [Mail chats](channels.md#mail-chats).
- **Tool-free sessions list no skills.** A chat stranger's session, for
  example, no longer learns the names of the owner's skills; it could never
  read them.
- **Going back to `0.3.4`.** `0.3.4` rejects `purpose` and `mail` in a
  `[channels]` table and does not know `[channels.email]`, so with either in
  `config.toml` it cannot start, and an automatic rollback would fail too.
  Before going back, log out every email account and every mail chat, or
  remove those tables and keys. An instance that never used mail is
  unaffected: SCV writes `purpose` only for a mail chat.

What changes for code that embeds SCV's crates:

- `scv-channels` has a default `email` feature (`ChannelKind::Email`,
  `ChannelCredentials::Email`, and `scv_channels::email`), and
  `AccountSettings` gains `purpose` and an opaque `mail` table; it is no
  longer `Eq`, since the table may hold floats.
- The hub gains `Link::register_as(Purpose)`, `Hub::purpose`,
  `Hub::is_mail_chat`, `Hub::notify_keyed`, `Hub::keyed_outcome`, and
  `Hub::register_mail`/`mail_counts`. `Hub::notify` and `send_question` refuse
  a mail chat with the new `NotifyError::WrongPurpose`.
- `scv-protocol` adds `Purpose`, `MailCounts`, `session.start`
  `system_prompt`, `channel_set` `purpose`, and `ComponentHealth` `purpose`
  and `mail`, all optional.
- New dependencies of `scv-channels` under `email`: `encoding_rs` for mail
  charsets, and `rustls`, `tokio-rustls`, `webpki-roots`, and `html2text`,
  which the workspace already used.

## Upgrading to 0.3.4

`0.3.4` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3,
so a planned restart from `0.3.3` checks the new release and can roll it back.

What changes for a person running SCV:

- **Agent defaults can distinguish ordinary and hard work.** `[agents.<name>]`
  `model` and `effort` now apply to every call that leaves those values out,
  while `hard_task_effort` tells the main agent which effort to pass when a
  task needs sustained reasoning. Defaults are filled before validation,
  approval, and background execution. ACP and nested-SCV conversations keep
  the settings they started with; one-process-per-turn CLIs receive defaults
  on every turn. See [delegated-agent defaults](tools.md#model-and-effort-values).
- **Prompts and checks show the complete choice.** Agent lines, the built-in
  `delegating` skill, `scv agents check`, and configuration output distinguish
  ordinary defaults from the hard-task effort, and validate both effort values
  against the agent's accepted format.
- `0.3.3` rejects `[agents.<name>] hard_task_effort`, so remove it before
  going back to `0.3.3` by hand.

What changes for code that embeds SCV's crates:

- `AgentAdapterConfig` carries an `AgentDefaults` value with `model`,
  `effort`, and `hard_task_effort`; `AgentTool::route` returns the arguments
  after defaults are applied.
- `scv_tools::AgentDefaults` and `choice::defaults_phrase` are the shared
  representation and prompt formatting for these settings.

## Upgrading to 0.3.3

`0.3.3` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3,
so a planned restart from `0.3.2` checks the new release and can roll it back.

What changes for a person running SCV:

- **SCV learns each agent's models from the agent.** Every ACP session's
  model and effort values are saved in `$SCV_HOME/state/agent-options/`, and
  later sessions list them in the `agent` tool. A model the list lacks is
  refused before a job starts, instead of starting a job that fails. See
  [model and effort values](tools.md#model-and-effort-values).
- **`scv agents check`** makes one short call to each installed agent the way
  SCV runs it. It shows how SCV reaches the agent, its version, the models and
  efforts it offers, and whether your configured model and effort work. See
  [checking delegated agents](tools.md#checking-delegated-agents).
- **A built-in `delegating` skill** tells the main agent how to choose an
  agent, model, and effort, how to write a brief, and what to do when a call
  fails. The system prompt asks it to read the skill before its first agent
  call. A `delegating` skill in your SCV skill directory replaces it. A
  project's agent skill of that name is no longer listed for the main agent.
- **`[agents.<name>] effort`** takes any value of letters, digits, `-`, and
  `_` that starts with a letter or digit, such as Codex's `ultra`, and the
  agent checks it. `0.3.2` accepts only `low`, `medium`, `high`, `xhigh`, and
  `max`, so change any other value before going back to `0.3.2` by hand. A
  rollback ignores `state/agent-options/`.
- The README recommends Feishu over WeChat for phones, and a Simplified
  Chinese README is added.

What changes for code that embeds SCV's crates:

- `SkillMap` values are `scv_tools::Skill` (`File` or `Builtin`) instead of
  paths.
- `AgentAdapterConfig` has an `options_file`, and `ToolsConfig` has a
  `precheck_agent_models` flag. `Layout` has an `agent_options(name)` path.
  `scv_tools` adds `reach`/`Reach`, `call_agent`, and the `agent_options`
  module.
- `scv_tools::valid_effort` checks that a value is well formed instead of
  checking it against `AGENT_EFFORTS`.

## Upgrading to 0.3.2

`0.3.2` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3,
so a planned restart from `0.3.1` checks the new release and can roll it back.

What changes for a person running SCV:

- **SCV remembers the owner's chats.** Each account owner's direct chat is
  logged under `$SCV_HOME/history/`, in episodes split by two quiet hours. A
  new session carries on from the open episode, so a conversation survives
  the daemon restarting or its session idling out, and the model can search
  and read earlier episodes with `chat_history`. `/new` in chat starts a
  fresh conversation. See [chat history](channels.md#chat-history).
- **Chat files are kept a year** instead of 7 days (`keep_days` now defaults
  to 365), and the owner can have one kept for good (`chat_keep`, "keep
  this").
- **A nearly full disk** (below `[history] min_free_percent`, 20%, of the
  disks holding chat files) is announced to the owner, and SCV saves no new
  files from chat until there is room.
- The new `[history]` table sets the episode gap, the free-space floor, and
  where kept files go. `0.3.1` rejects that table, so remove it before
  going back to `0.3.1` by hand. A rollback also shows `history/` under "Not
  used by SCV" and removes chat media older than 7 days.

What changes for code that embeds SCV's crates:

- `session.start` takes an optional `chat` log reference
  (`scv_protocol::ChatLog`), and `scv_client::history` reads and writes the
  log. `AccountRun` has an `episode_gap`, `Layout` a `history()` path, the
  tools configuration a `chat_history` entry, and the channel hub a low-disk
  flag.

## Upgrading to 0.3.1

`0.3.1` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3,
so a planned restart from `0.3.0` checks the new release and can roll it back.
Configuration is unchanged.

What changes for a person running SCV:

- **One `agent` tool delegates to every agent.** The model calls `agent` with
  the agent's name (`claude`, `codex`, `grok`, `dsh`, `pi`, or `scv`) instead
  of one `agent_<name>` tool per agent; the old names are gone, with no
  aliases. `agent_wait`, `agent_status`, and `agent_cancel` keep their names.
  A call that names no agent uses the first offered agent in `[agent] prefer`.
  Update skills, `AGENTS.md` files, and prompts that name `agent_codex` or
  `agent_claude` to say the `agent` tool with agent `codex` or `claude`.
  DeepSeek Harness over ACP no longer takes `model` or `effort`, which its
  configuration already rejected.
- **SCV's own WeChat messages** (notices, busy and failure replies, and
  questions) go out in a Markdown code block that starts with `system msg:`.
  Model answers are sent exactly as written, and Feishu is unchanged.
- **Planned restarts** stop waiting for a live delegated agent that is between
  turns, but keep waiting while a nested SCV's own background jobs run or wait
  to be reported. The daemon that schedules a restart applies its own rules,
  so the restart into `0.3.1` still follows those of `0.3.0`.
- **`scv status`** counts live agents between turns as idle
  (`Delegations: 1 running, 2 idle, …`).

A rollback to `0.3.0` reads everything `0.3.1` saves; its notice of stopped
background jobs names a job `0.3.1` recorded as `agent` instead of the agent.

What changes for code that embeds SCV's crates:

- `JobChange` has an `agent` field, and its `tool` is `agent` for jobs the
  `agent` tool starts. Readers fall back to the `agent_` prefix of `tool` for
  jobs from older releases.
- Daemon status adds `idle` to the delegation summary, and
  `idle_since_unix_seconds` and `background_jobs` to each delegation entry.

## Upgrading to 0.3.0

`0.3.0` keeps the instance layout (`CONFIG_LAYOUT` 1) and protocol version 3,
so a planned restart from `0.2.1` checks the new release and can roll it back,
and existing credentials, delivery state, and settings are read unchanged. A
running TUI still needs a restart after the update.

What changes for a person running SCV:

- **Chat channels answer only the account owner by default.** An account
  table without `senders` answers only its owner; messages from anyone else
  are checkpointed with no reply and no model turn. Set
  `senders = "anyone"` in `[channels.<channel>.<account>]` (or run
  `scv channels run <channel> --senders anyone`) to answer every sender
  again. An account whose sign-in recorded no owner answers nobody;
  `scv channels status` shows a `Note:` line for it. A release before `0.3.0`
  refuses an account table that sets `senders`, so remove the key before
  rolling back.
- **Voice messages without a platform transcript** (every Feishu voice
  message, and a WeChat one whose transcript is missing) get the fixed reply
  "SCV cannot listen to voice messages yet. Please type your message
  instead." instead of a model turn.
- **`scv confirm`** asks the owner a yes-or-no question in chat, and the
  feature flow's `publish.sh` uses it when an agent SCV delegated to
  publishes. The daemon and CLI that ask must both be `0.3.0` or newer, so
  `0.3.0` itself is published from a terminal.
- **`[agents.<name>]`** accepts `model` and `effort` defaults for the work
  its `use_for` matches.
- **Log targets** of the WeChat and Feishu transports are now
  `scv_channels::wechat` and `scv_channels::feishu`; update any `RUST_LOG`
  filter that names `scv_clawbot` or `scv_feishu`.

What changes for code that embeds SCV's crates:

- `scv-clawbot` and `scv-feishu` are the `wechat` and `feishu` modules of
  `scv-channels`, behind Cargo features of the same names (both on by
  default), with one `Channel` trait and `scv_channels::run` for every
  account. The old crates' final `0.3.0` releases contain no code.
- `scv-server` no longer exports the CLI's administration helpers, and every
  crate takes the instance `Layout` explicitly instead of reading `SCV_HOME`.
- Errors are typed: `scv_protocol::ErrorCode` and `ToolErrorKind`,
  `scv_core::ToolError { kind, message }` and `ToolOutput::failure`, and
  `scv_client::ControlError`. Background jobs report typed `JobStatus` and
  `JobChange` values on `tool.completed.jobs`.
- Items no other crate uses are no longer `pub`.

## Upgrading to 0.2.0

`0.2.0` changes where an instance keeps its files, as described in
[instance layout](configuration.md#instance-layout), and reads only the new
layout: it moves nothing itself. A daemon started on an old home finds no
channel accounts or agent sign-ins, logs a warning for each old path, and
`scv config show` lists them under "Not used by SCV". Move them once, with the
daemon stopped, for each instance home (`~/.scv` and any `--scv-home`):

```bash
scv stop                                   # or: systemctl --user stop scv.service
cp -a ~/.scv ~/.scv.bak-0.1                # keep a copy until 0.2.0 works
cd ~/.scv
mv adapters agents                         # delegated agents' homes
mkdir -m 700 -p credentials state/channels
for channel in wechat feishu; do
  [ -d channels/$channel/accounts ] && mv channels/$channel/accounts credentials/$channel
  [ -d channels/$channel/state ] && mv channels/$channel/state state/channels/$channel
done
[ -d run/delegations ] && mv run/delegations state/
[ -d run/conversations ] && mv run/conversations state/
rm -f server.sock server.lock              # recreated under state/
```

Then write each `channels/<channel>/settings/<account>.json` as a table in
`config.toml` and remove the old `channels/` and `run/` directories. For
example `{"enabled":true,"workspace":"/srv/work","remote_tools":"owner"}` for
`wechat/default` becomes:

```toml
[channels.wechat.default]
enabled = true
workspace = "/srv/work"
remote_tools = "owner"
```

The nested SCV's home moves with `agents/` (`agents/scv`); inside it, rename
its own `adapters` to `agents` too. Old lock files (`channels/*/locks`,
`channels/*/transactions`) are not needed. State from before `0.1.35` lives in
`clawbot/` rather than `channels/wechat/`, with the same subdirectories.
Start the daemon with `scv start --workspace ...` and check `scv config show`
and `scv channels status`. To go back, stop the daemon, restore the copy, and
install the earlier release.

## Publication and checks

The end-to-end landing flow for this repository is the `feature-flow` agent
skill at `.agents/skills/feature-flow/SKILL.md`. Codex reads it from
`.agents/skills`, and Claude Code from the `.claude/skills` symlink to the same
directory. The flow develops in a sibling worktree and passes the checks
below. It lands each change on `origin/main` as one squashed commit, through a
`gh` pull request when `gh` is signed in or a fast-forward push otherwise, then
publishes, installs the release, and restarts the local daemon. Its scripts
cover the steps that are easy to get wrong:

- `publish.sh`: a resumable publish in dependency order; run by an agent SCV
  delegated to, it first asks the owner yes or no in chat (`scv confirm`)
  and publishes only on yes. An installed `scv` older than 0.3.0 cannot ask,
  so the release that introduces `scv confirm` is published from a terminal;
- `deploy.sh`: keep the running binary as `<binary>.prev`, install, and ask
  the daemon to restart when idle (`scv restart --when-idle`), which checks
  and, on failure, rolls back the release; from a terminal it then waits for
  the new version and connected accounts. A daemon older than that (0.2.0
  and earlier) is restarted by the script itself, without the watchdog;
- `host.sh`: run landing commands from agents that SCV started with a private
  home.

Publish from a clean verified checkout after authenticating with `cargo login`.
Wait for each dependency version to become available before publishing its
dependents:

```bash
cargo publish --locked -p scv-core
cargo publish --locked -p scv-protocol
cargo publish --locked -p scv-client
cargo publish --locked -p scv-provider-openai
cargo publish --locked -p scv-tools
cargo publish --locked -p scv-channels
cargo publish --locked -p scv-server
cargo publish --locked -p scv-tui
cargo publish --locked -p scv-cli
```

Required checks remain:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check advisories bans licenses sources
cargo build --release --locked
git diff --check
```

`cargo-deny` is required in CI and runs locally when installed. These commands
are release requirements, not a record of a successful run.

## Compatibility policy

- The JSONL protocol used by both Unix sockets and stdio is versioned
  independently from the crate version; version 2 added daemon management and
  version 3 added `tool.progress`. Clients and the server share one binary, so a
  running TUI from an older release must be restarted after an update.
- Additive object fields do not change the protocol version.
- Removing a field, changing its meaning, or changing message ordering requires
  a protocol version increase.
- Configuration rejects unknown keys in v0.x so misspellings do not silently
  weaken behavior.
- Rust traits are extension seams but do not promise a stable third-party ABI
  before 1.0.
- Linux and macOS are release-gated; other platforms are best effort until they
  join the CI matrix.
