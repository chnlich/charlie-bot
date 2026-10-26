# Environment variables

Baseline: the Python tree at `4521a51b2`. Section 1 covers what the server reads, section 2 what the
`charliebot` CLI reads, and sections 3 to 5 what the server writes into the environment of every process it
starts, with the module that sets each variable. Found by grepping `src/` and `server.py` for `os.environ`,
`environ.get`, `getenv` and `env[...] =`. **UNVERIFIED** marks a statement not confirmed from source, with the
reason.

## 1. Variables the server process reads

| Variable | Reader | Effect |
| --- | --- | --- |
| `CHARLIEBOT_HOME` | `src/infra/home.py::_resolve_home` (the only resolver); `src/features/terminal/terminal.py` checks whether it is set | Selects the profile home; default `/home/<user>/.charliebot`. A set value must start with `/` or `~` (`files.md` section 1). |
| `HOME` | `home.py` (cache key, `Path.home()`), every `expanduser` | Base of the default home, `~/workspace`, `~/worktrees`, `~/.ssh/config`, `~/.claude`, `~/.codex`, `~/.local/bin` and `~/.opencode/bin/opencode` |
| `PATH` | `agent_environment.agent_environment`; `shutil.which` for `claude`, `codex`, `gemini`, `agy`, `charlie-code`, `tmux`, `sacct`, `git`, `node` | Binary resolution; rewritten once at start (section 3) |
| `VIRTUAL_ENV`, `UV_RUN_RECURSION_DEPTH` | `agent_environment` | Removed from the process environment at start |
| `_VFORKSPAWN_TEST_EXEC_DELAY_MS` | `src/runtime/agent_process/_vfkspawn.c:170` (the vfork stub, read at each spawn) | Regression-test hook only: a value that `strtol` reads as above 0 holds the spawn child for that many milliseconds before `execve` (`_vfkspawn.c:74-80`). No launcher sets it. The Rust spawn path does not load the stub. |
| `CLAUDE_CONFIG_DIR` | `src/backends/claude_code/login_dirs.py::claude_config_dir` | The default Claude login directory (else `/home/<user>/.claude`) for resume lookup, usage logs and the quota panel |
| `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, `CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT` | `src/backends/claude_code/claude_launch.py::headless_claude_env`, read per call | Forwarded to headless Claude children; a host value beats the per-run-kind default. The first three also drive the declared-window logic. |
| `NO_PROXY` | `src/runtime/agent_process/base.py::apply_proxy_env` | Merged with `localhost,127.0.0.1,::1` when a backend option sets `proxy_url` |
| `GEMINI_API_KEY`, `GOOGLE_API_KEY` | `base.strip_google_api_keys` | Removed from gemini and antigravity children so they bill through OAuth |
| `TMUX` | `src/runtime/agent_process/pty_common.py` | Removed from tmux client invocations |
| `CHARLIEBOT_SESSION_ID` | none in the server; it is stripped from children (section 4) | A server started from inside an agent session would otherwise hand its id down |
| `PYDANTIC_DISABLE_PLUGINS` | `src/__init__.py` | `os.environ.setdefault("PYDANTIC_DISABLE_PLUGINS", "__all__")` at import, so every child inherits `__all__` unless the host set another value |

Launcher only (`scripts/start-server.sh`, not the server): `CHARLIEBOT_LOG_DIR` (default `/tmp/charliebot-logs`)
and `PYTHONUNBUFFERED=1`, which the script sets for the server process.

## 2. Variables the `charliebot` CLI reads

| Variable | Reader | Effect |
| --- | --- | --- |
| `CHARLIEBOT_HOME` | `home.py` | Same home rule as the server; selects `config.yaml`, `credentials.yaml` and the port cache |
| `CHARLIEBOT_SESSION_ID` | `src/runtime/cli/common.py::resolve_session_id`, `_caller_headers`; `src/features/artifacts/cli.py` | The caller's own session. Sent as `X-CharlieBot-Caller-Session`. It wins over a cwd-derived session; a `--session` that disagrees with it is an ambiguity error (exit 2). |
| `CHARLIEBOT_RUN_TOKEN` | `src/runtime/run_token.py::load_run_token` (from `cli/common.py`) | Sent as `Authorization: Bearer <token>`; without it the CLI sends `Bearer <credentials.charliebot.access_key>` when one is set |
| `CHARLIEBOT_VENV_BIN` | `src/runtime/agent_entry_points/charliebot` (shell shim) | `exec "$CHARLIEBOT_VENV_BIN/charliebot"`; the shim fails with `is unset; the CharlieBot server sets it for agent processes` when it is missing |
| `COLUMNS`, `PYTHON_COLORS`, `NO_COLOR`, `FORCE_COLOR`, `TERM` | `src/infra/help_formatter.py` | Help text width and color |

## 3. Server environment rewrite at start (`src/runtime/agent_environment.py::apply_agent_environment`)

Every agent child copies the server's `os.environ`, so `server.py::main` rewrites it once before anything spawns:

- `VIRTUAL_ENV` and `UV_RUN_RECURSION_DEPTH` are removed.
- Every `PATH` entry that resolves (`realpath`) to the venv's `bin` directory is removed.
- `ENTRY_POINT_DIR` (`<repo>/src/runtime/agent_entry_points`, holding the `charliebot` shim) is prepended to
  `PATH` unless it is already an entry.
- `CHARLIEBOT_VENV_BIN=<venv>/bin` is set, so the shim finds the real entry point.

The effect: an agent's `charliebot` resolves to this server's venv, while no venv interpreter is on `PATH` for
`uv` to install into.

## 4. Agent child environments

The pieces below are applied in order. "Inherited" means the rewritten server environment from section 3.

### 4.1 Common edit (`src/runtime/launch_loop.py::child_env`)

Applied to manager turns and worker, review, iteration and scheduled-step runs:

| Variable | Change | Setter |
| --- | --- | --- |
| `CHARLIEBOT_SESSION_ID` | removed (the inherited value) | `launch_loop.child_env` |
| `CLAUDECODE` | removed | `src/backends/claude_code/claude_code.py::claude_child_env`, registered through `backend_lifecycle_registration.register_child_env` and applied to every backend type |
| `CLAUDE_CODE_DISABLE_AUTO_MEMORY` | `1` | same |

### 4.2 Run identity (`src/runtime/task_execution.py::TaskExecutionAdapter._child_env`)

Every v2 launch, both manager turns and workers, adds the following after the common edit:

| Variable | Value |
| --- | --- |
| `CHARLIEBOT_SESSION_ID` | the run's own session id |
| `CHARLIEBOT_RUN_TOKEN` | signed run token (below) |
| `CHARLIEBOT_HOME` | `str(cfg.charliebot_home)`, the resolved home, so the child's CLI finds this instance |

A missing `credentials.charliebot.access_key` refuses the launch: `"run-token signing requires credentials.yaml
charliebot.access_key; agent child environments cannot be built without it"`.

Run token format (`src/runtime/run_token.py`):
`b64url(payload) + "." + b64url(HMAC_SHA256(access_key, b64url(payload)))`.

- `payload` is `json.dumps({"agent", "run_id", "session_id"}, separators=(",", ":"), sort_keys=True)` with the
  default `ensure_ascii=True`, so a non-ASCII session name is `\uXXXX`-escaped, then UTF-8 encoded.
- The HMAC key is the UTF-8 access key, and the HMAC message is the ASCII text of `b64url(payload)`, not the raw
  JSON. Verification compares with `hmac.compare_digest`.
- `b64url` is unpadded URL-safe base64.
- `agent` is the session name, or `"worker"` when the name is empty.

The server verifies the token on every request that carries it. A Rust server must sign and verify the same way,
because a child launched by one server may call the other after a restart.

### 4.3 Manager turns (`src/runtime/master_cc_run.py::_build_master_env`)

| Variable | Value | Setter |
| --- | --- | --- |
| `CHARLIEBOT_SESSION_ID` | the manager's session id (then overwritten by 4.2 with the same value) | `_build_master_env` |
| `GIT_CEILING_DIRECTORIES` | `<home>` | `_build_master_env` |

The working directory is `<home>/sessions/<sid>`.

### 4.4 Worker-class runs (`src/runtime/worker.py::Worker.run`)

| Variable | Value | Setter |
| --- | --- | --- |
| `PATH` | `<repo>/src/runtime/git_stash_guard:` prepended | `Worker.run` (the directory holds a `git` wrapper that refuses `git stash` write forms and passes everything else to the real git) |

The working directory is the run's worktree.

### 4.5 Backend edits (`<backend>._prepare_env`, applied last)

| Backend type | Variables | Module |
| --- | --- | --- |
| `cc-claude` | First removes the inherited gateway set: `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_CUSTOM_HEADERS`, `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, and `CLAUDE_CODE_SUBAGENT_MODEL`. Then sets `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`; `BASH_DEFAULT_TIMEOUT_MS=900000`; `BASH_MAX_TIMEOUT_MS=900000`; `CLAUDE_CODE_DISABLE_ADVISOR_TOOL=1`; `CLAUDE_CODE_AUTO_COMPACT_WINDOW=433000` for each run kind unless the host set it; host values of the four forwarded variables (section 1); `CLAUDE_CONFIG_DIR=<account config_dir, ~ expanded>` when the run uses a pool account. | `claude_code.without_gateway_env`, `ClaudeCodeBackend._prepare_env`, `claude_launch.headless_claude_env` |
| `cc-anthropic-compatible` | the cc-claude set (no pool account), plus `ANTHROPIC_BASE_URL=<api_base without trailing />`, `ANTHROPIC_AUTH_TOKEN=<credentials.<credential>.api_key>` (`no-auth` when the option has no credential), and `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `CLAUDE_CODE_SUBAGENT_MODEL` all `=<model>`. Non-empty `extra_body` sets `CLAUDE_CODE_EXTRA_BODY` to its JSON text; that variable is gateway-only, and `without_gateway_env` strips it from the official path (`claude_code.py:462-463`). Non-empty `headers` sets `ANTHROPIC_CUSTOM_HEADERS` to newline-joined `name: value` entries. The entry's `context_window` sets `CLAUDE_CODE_AUTO_COMPACT_WINDOW` and `CLAUDE_CODE_MAX_CONTEXT_TOKENS` to that window, removes `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN`, and sets `CLAUDE_CODE_THINKING_DISPLAY_UPDATES=0`. | `claude_code.AnthropicEndpointBackend._prepare_env`, `src/backends/anthropic_compatible/factory.py` |
| `cc-openai-compatible` | the cc-claude set (no pool account), plus `ANTHROPIC_BASE_URL=http://localhost:<server.port>/api/anthropic-proxy/openai-compatible/<option id>`, `ANTHROPIC_AUTH_TOKEN=<credentials.charliebot.access_key>`, and the five model variables | `src/backends/openai_compatible/factory.py` |
| `codex` | `/home/<user>/.local/bin` prepended to `PATH` unless present | `src/backends/codex/codex.py` |
| `gemini` | `GEMINI_API_KEY` and `GOOGLE_API_KEY` removed | `src/backends/gemini/gemini_cli.py` |
| `antigravity` | `GEMINI_API_KEY` and `GOOGLE_API_KEY` removed; `/home/<user>/.local/bin` prepended to `PATH` | `src/backends/antigravity/antigravity_cli.py` |
| `charlie-code` | `/home/<user>/.local/bin` prepended to `PATH`; `CHARLIE_CODE_API_KEY=<credentials.<credential>.api_key>` when the option names a credential; `HTTP_PROXY`, `HTTPS_PROXY` (`=proxy_url`) and merged `NO_PROXY` when it sets `proxy_url` | `src/backends/charlie_code/charlie_code.py` |
| `opencode` | The turn's own process (`/usr/bin/sleep infinity`) gets the env unchanged. The same env, plus proxy variables when `proxy_url` is set, plus `BASH_ENV=<repo>/src/backends/opencode/cgroup_join.sh` and `CHARLIEBOT_SESSION_CGROUP=<cgroup dir>` when the session has a cgroup, is sent to the shared server as the session's shell environment (`PUT /api/session/<id>/environment`), so the agent's tool shells see it. | `src/backends/opencode/opencode.py::_put_environment` |

## 5. Other processes the server starts

| Process | Environment | Setter |
| --- | --- | --- |
| `opencode serve` (shared, one per `proxy_url`) | inherited, minus `CHARLIEBOT_RUN_TOKEN` and `CHARLIEBOT_SESSION_ID`, plus `XDG_DATA_HOME=<home>/opencode/data`, `XDG_STATE_HOME=<home>/opencode/state`, `XDG_CACHE_HOME=<home>/opencode/cache`, `OPENCODE_SERVER_PASSWORD=<random token_urlsafe(32)>`, `OPENCODE_CONFIG_CONTENT=<headless config JSON>`, `OPENCODE_DISABLE_FFF=1` (the file index is unused), and proxy variables | `src/backends/opencode/server.py::OpenCodeServer.server_env` |
| one-shot text calls (session namer, explain, page cold read) on the base backend (gemini, antigravity, charlie-code) | inherited `os.environ` with no `child_env` edit and no run identity, then the backend's own `_prepare_env` (the call goes through `run()`); working directory is a fresh `/tmp/charliebot-oneshot-*` | `base.AgentBackend.one_shot_text` |
| `claude -p` one-shot | cc-claude: inherited `os.environ` with the gateway set removed (`ClaudeCodeBackend._one_shot_env`, `without_gateway_env`); no headless or endpoint variables. cc-anthropic-compatible: the entry's gateway environment, `_prepare_env(os.environ)` (`_one_shot_env`, commit `6356179b7`). cc-openai-compatible inherits the method; UNVERIFIED after `6356179b7`. | `ClaudeCodeBackend.one_shot_text` |
| `codex exec` one-shot | inherited, then the codex `_prepare_env` | `CodexBackend.one_shot_text` |
| `opencode run --standalone` one-shot | inherited, plus `XDG_*_HOME` under a fresh `/tmp/charliebot-opencode-oneshot-*`, `OPENCODE_CONFIG_CONTENT={"permission":{"*":"deny"}}` plus a `providers` block with each `context_limit` of the `opencode` options (`src/backends/opencode/opencode.py:540-548`), and proxy variables | `OpenCodeBackend.one_shot_text` |
| Claude compaction (`claude --resume ...`) | The account compaction uses `claude_supervisor_env(os.environ)` (removes `CHARLIEBOT_SESSION_ID` and applies 4.1), `headless_claude_env(run_kind)` and `CLAUDE_CONFIG_DIR=<account config_dir>`. Idle compaction builds the session's backend environment for its run kind. | `src/backends/claude_code/claude_compaction.py`, `claude_idle_compaction.py` |
| Perfetto merge helper | inherited plus `PYTHONPATH=<repo root>` | `src/features/trace/trace_merge.py` |
| tmux (terminal and PTY attach) | inherited minus `CHARLIEBOT_SESSION_ID` (and minus `TMUX` for client commands); the attach client gets `TERM=xterm-256color`; a new terminal pane gets `-e CHARLIEBOT_HOME=<home>` when `CHARLIEBOT_HOME` is set | `pty_common.py`, `terminal.py` |
| `code-server --config <path>` | inherited | `src/features/code_server/api.py` |
| `git`, `ssh`, `sacct`, `node` helpers | inherited | various |

**UNVERIFIED:** whether any one-shot caller passes a session-scoped env instead of `os.environ`; the callers in
`src/runtime/autonamer.py` and `src/features/explain/explain.py` were not traced line by line.
