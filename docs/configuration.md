# Configuration

Status: final design for v0.1

Run `scv config init` on first use to create the user file from `config.example.toml`. Select a profile with `provider.active` or `--provider`.

SCV merges configuration in this order, from lowest to highest precedence:

1. built-in defaults;
2. `$SCV_HOME/config.toml` or `~/.scv/config.toml`;
3. `<workspace>/.scv/config.toml`;
4. documented environment variables;
5. command-line flags.

Unknown keys and invalid values are startup errors. Project configuration is
treated as untrusted input: it cannot contain credentials or disable an
interactive approval required by user-level policy.

## Schema

```toml
[provider]
active = "openai"

[providers.openai]
kind = "openai-compatible"
model = "gpt-4.1-mini"
base_url = "https://api.openai.com/v1"
api_key = "sk-your-key"
api_key_env = "OPENAI_API_KEY" # optional fallback
timeout_seconds = 120

[providers.custom]
kind = "openai-compatible"
model = "your-model"
base_url = "https://provider.example/v1"
api_key_env = "CUSTOM_PROVIDER_KEY"
headers = { "X-Organization" = "example" }
timeout_seconds = 120

[agent]
max_steps = 32
system_prompt = "You are SCV, a concise and careful coding agent."

[session]
max_history_bytes = 16777216
max_messages = 10000

[context]
max_tokens = 128000
reserve_output_tokens = 8192
safety_margin_tokens = 2048
bytes_per_token = 3
summary_max_chars = 6000

[tools]
approval_policy = "on-risk"
command_timeout_seconds = 120
output_limit_bytes = 65536
max_read_bytes = 262144
max_write_bytes = 1048576

[protocol]
max_client_frame_bytes = 1048576
max_server_frame_bytes = 8388608

[tui]
max_transcript_bytes = 8388608
max_transcript_items = 10000
max_prompt_history_bytes = 1048576
max_prompt_history_items = 200

[provider_limits]
max_sse_event_bytes = 1048576
max_response_bytes = 4194304
max_assistant_bytes = 1048576
max_tool_calls = 32
max_tool_arguments_bytes = 262144

[skills]
user_dir = "~/.scv/skills"
project_dir = ".scv/skills"
max_skills = 128
max_skill_bytes = 262144

[agents.claude]
command = "claude"
args = ["-p"]

[agents.codex]
command = "codex"
args = ["exec"]

[agents.pi]
command = "pi"
args = ["-p"]
```

`agents.*.args` is an argument vector, not a shell string. SCV appends the
delegated prompt as the final argument and runs the child in the session
workspace. The three built-in adapters are enabled when their executable is
available; attempting to call a missing adapter returns a clear tool error.

`provider`, `agents.*`, and `skills.user_dir` are accepted only from built-in,
user, explicit `SCV_CONFIG`, environment, and CLI layers. Project configuration
cannot change a model endpoint, credential-variable name, user skill root,
executable, or fixed arguments. A native-agent approval shows the resolved
absolute executable, the complete fixed argument vector, the workspace, and
the bounded prompt argument before launch.

Project configuration may lower context, size, output, and timeout limits; make
approval policy stricter; set `skills.project_dir` within the workspace; and
append project instructions. Attempts to weaken a limit or set a user-only key
are startup errors rather than ignored fields.

Cross-field validation requires every specific content limit plus serialization
overhead to fit its protocol frame limit, tool arguments to fit provider
responses, and all count/byte limits to be positive. SCV fails startup with the
conflicting key names instead of silently clamping values.

Approval policies are:

- `on-risk` (default): approve ordinary `read` calls; prompt for secret-like
  reads, `write`, `bash`, and every `agent_*` tool;
- `always`: prompt for every tool;
- `never`: deny tools whose declared risk is not read-only.

The daemon uses `on-risk` when no policy is supplied. Set the policy explicitly
for a managed daemon with a global flag, for example
`scv --approval-policy always start --workspace /path/to/workspace`.

Starting or restarting the daemon also checks whether the current user has
usable sudo authorization. An interactive start without it asks whether to
continue; `--allow-sudo` invokes sudo authentication first. That option only
validates the user's existing operating-system policy; SCV never edits sudoers
or grants privileges. A non-interactive start without verified sudo
authorization fails so unattended jobs do not silently run with reduced
capability.

The updater reads an optional Cargo registry index from the user configuration:

```toml
[update]
index_url = "https://mirrors.ustc.edu.cn/crates.io-index"
```

`SCV_CARGO_INDEX_URL` and `scv update --index-url URL` override the configured
value in that order. Project configuration cannot select an update registry.
SCV passes the URL to Cargo, which remains responsible for registry
authentication and downloads.

Project configuration may make policy stricter but not weaker than user
configuration. A command-line flag may weaken policy because it is an explicit
choice for that invocation.

## Environment variables

SCV v0.1 reads:

- `SCV_HOME` for the user configuration, skills, daemon socket, and ClawBot state
  root (default `~/.scv`);
- `SCV_MODEL`;
- `SCV_BASE_URL`;
- `SCV_API_KEY_ENV` (the name of the credential variable, not its value);
- `SCV_CONFIG` for one additional explicit configuration file;
- `SCV_CARGO_INDEX_URL` for the update registry;
- the credential variable named by `provider.api_key_env`;
- `RUST_LOG` for diagnostics.

Secrets are never included in diagnostics, protocol events, approval summaries,
or tool results.

## Daemon and component settings

`scv-client` resolves the default socket as `$SCV_HOME/server.sock`, normally
`~/.scv/server.sock`. The TUI and daemon control commands use this same path.
`scv status` queries the running server; `scv reload` immediately reconciles
saved accounts and component settings without restarting unrelated sessions.
The daemon also reconciles on startup and every two seconds.

ClawBot credentials live in `clawbot/accounts/<account>.json` and durable
delivery state in `clawbot/state/<account>.json` under the same root. Per-account
settings are separate from project TOML, at
`$SCV_HOME/clawbot/settings/<account>.json`:

```json
{"enabled":true,"workspace":"/absolute/path/to/workspace"}
```

Missing settings default to `enabled: true`; an omitted or null `workspace`
uses the daemon workspace. An explicit workspace must be an existing absolute
directory. Saved accounts autostart when enabled, but QR login is always
explicit. Logging in again preserves a saved disabled setting.

Account settings reject unknown keys. The daemon reads credentials and settings
together under the account transaction lock. Login can rotate a token for the
same known bot/user identity and normalized API origin while preserving delivery
state. A different identity/origin requires explicit logout first; legacy
credentials without both IDs are conservatively bound to their token and also
require logout before replacement with an identified account.
A busy transaction during the snapshot defers reconciliation; the current
instance keeps running until a later pass can read the account.

`scv clawbot run --account NAME --workspace PATH` persists enablement and the
resolved workspace through the live daemon, then returns. `scv clawbot stop
--account NAME` persists `enabled: false` and joins the instance while retaining
credentials. Credential or settings changes join the old instance before a
replacement starts. `scv clawbot logout --account NAME` requires a live daemon
and removes credentials, delivery state, and settings only after joining.

For an offline opt-out, create or edit the account settings to contain
`{"enabled":false}` before starting the daemon. Keep ClawBot directories mode
`0700` and files mode `0600`; account, settings, and state files must be private
regular files. Invalid or inaccessible settings fail that account closed.
Project configuration cannot select accounts, component workspaces, or remote
authority. See [ClawBot](clawbot.md) for the lifecycle and status contract.

`scv update` installs the published binary and restarts an active systemd user
daemon. A foreground daemon requires an explicit restart; its in-memory code
does not change when the executable is replaced.
