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

Project configuration may make policy stricter but not weaker than user
configuration. A command-line flag may weaken policy because it is an explicit
choice for that invocation.

## Environment variables

SCV v0.1 reads:

- `SCV_MODEL`;
- `SCV_BASE_URL`;
- `SCV_API_KEY_ENV` (the name of the credential variable, not its value);
- `SCV_CONFIG` for one additional explicit configuration file;
- the credential variable named by `provider.api_key_env`;
- `RUST_LOG` for diagnostics.

Secrets are never included in diagnostics, protocol events, approval summaries,
or tool results.
