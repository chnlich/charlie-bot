# Agent processes (summary depth)

Baseline: the Python tree at `d113f2c4d`. For each backend package under `src/backends/`, this document gives the
executable, the module and function that build argv, env, cwd and stdin, and the output format the server parses.
Environment details are in `env.md` and transport files in `files.md` section 6.4. **UNVERIFIED** marks a
statement not confirmed from source, with the reason.

## 1. Common launch path (`src/runtime/agent_process/base.py::AgentBackend.run`)

The template method runs these steps in order:

1. `_prepare_cwd(cwd)`: a manager turn writes its instructions into the session directory as
   `_INSTRUCTIONS_TARGET` (`CLAUDE.md` or `AGENTS.md`). A worker or review run writes them to
   `<run dir>/harness_instructions.md` instead and leaves its worktree alone.
2. Transport: `log_dir` is the run directory, or a temp `/tmp/charliebot-run-*` for an unowned call.
   `_rotate_stale_transport` moves a non-empty previous `agent.raw.ndjson` aside. `_prepare_transport(log_dir)`
   follows.
3. `_build_command(prompt)` builds argv, `_stdin_prompt(prompt)` gives stdin text or None, and
   `_prepare_env(env)` gives the env.
4. Spawn (`src/runtime/agent_process/spawn.py::spawn_subprocess`): `cwd`, `start_new_session=True`, stdout to
   `agent.raw.ndjson` and stderr to `agent.stderr.log` (`O_APPEND`, 0644), and stdin a pipe when there is stdin
   text, else `/dev/null`.
   - No preexec and no pdeathsig, so the child outlives the server and the vfork fast path holds.
   - Right after exec, the parent raises the child's nice by 10 and writes its pid into the session cgroup
     (`_apply_turn_tree_limits`).
5. `(pid, pid_start)` is pinned from `/proc/<pid>/stat` and reported through `on_spawn`. The run record stores it.
6. Read: `tail_follow_events(raw_path, translate=self.translate_event, cursor=agent.raw.cursor, ...)` tails the raw
   file line by line. Each line is parsed as JSON and passed to `translate_event`, which returns Claude-Code-shaped
   event dicts, and the cursor checkpoints the consumed offset. After a `result` event it waits up to 90 s
   (`_POST_RESULT_TIMEOUT`) for exit, then cleans up within 30 s.
   - For a `result` event with `is_error: true`, `worker.py` raises `QuotaExhaustedError` for integer status 429 or
     at least 500, or when no status is present and the result matches a temporary failure pattern.
     That error cools the selected member and starts pool failover. A non-null status outside 429 and 5xx skips
     result-text matching.
7. `exit_code` and the stderr tail are recorded; a cgroup memory kill is attributed from `memory.events`.

Piped variant (`_spawn_piped_and_pin_identity`, used by antigravity and the opencode turn process): stdin
`/dev/null`, stdout and stderr pipes, and `PR_SET_PDEATHSIG`, so the child dies with the server. These types set
`restart_reattach=False`.

Who builds the inputs:

- Manager turns: `src/runtime/master_cc_run.py`. The cwd is `<home>/sessions/<sid>`, the env comes from
  `_build_master_env` plus `extra_env`, and the resume arguments from `_route_resume_session`.
- Worker-class runs: `src/runtime/worker.py::Worker`. The cwd is the worktree (`<paths.worktree_dir>/<branch with
  / replaced by ->`), and the env is `launch_loop.child_env` plus `extra_env` plus the stash-guard `PATH`.
- Backend construction goes through each type's registered factory (`<package>/factory.py::build`), which receives
  the option, the config and lifecycle keyword arguments (`resume_session_id`, `extra_flags`, `log_dir`,
  `instructions_*`, `cgroup_session_id`, `on_spawn`, `buffer_limit`).

Resume styles (`backend_type_registration`, from each package's `register()`):

| Type | `resume` | `restart_reattach` | Other traits |
| --- | --- | --- | --- |
| `cc-claude` | `cli_flag` (`--resume <id>` in argv) | true | `preassigned_session_id` (argv carries `--session-id <uuid>` chosen by CharlieBot), `manual_compact` |
| `cc-anthropic-compatible`, `cc-openai-compatible` | `cli_flag` | true | |
| `codex` | `native_id` (factory argument `resume_session_id`) | true | `family_prefix="codex"` |
| `gemini`, `charlie-code` | `native_id` | true | |
| `antigravity`, `opencode` | `native_id` | false | |

## 2. Per backend

### `src/backends/claude_code` (type `cc-claude`)

- **Executable:** `claude` from `PATH`, or `option.cli_binary`.
- **argv** (`claude_code.py::ClaudeCodeBackend.__init__`, `_build_command`):
  - fixed head (`BASE_COMMAND`): `claude -p --output-format stream-json --verbose --dangerously-skip-permissions
    --disallowed-tools <HEADLESS_DISALLOWED_TOOLS>`;
  - then `[--session-id <uuid>]`, `[--model <model>]`, `[--effort <effort>]`;
  - then `--settings <compact JSON>` with `disableClaudeAiConnectors: true` and `syncClaudeAiSkills: false`; it also
    has `disableBundledSkills: true` except for manager turns, a manager-turn `SessionStart` hook for matcher
    `compact`, and `fastMode: true` when the option enables it;
  - then the lifecycle's extra flags, `--resume <id>` first when resuming;
  - for a worker or review run, `--append-system-prompt-file <run dir>/harness_instructions.md`.
  - `HEADLESS_DISALLOWED_TOOLS` is `Monitor,ScheduleWakeup,CronCreate,CronDelete,CronList,Agent,Workflow,TaskCreate,
    TaskGet,TaskUpdate,TaskList,TaskStop,TaskOutput,SendMessage,ListAgents,ShareOnboardingGuide`. Review runs add
    `Skill`; other run kinds add `ReportFindings`.
  - A manager turn adds `--exclude-dynamic-system-prompt-sections`; it keeps machine-specific sections out of the
    system prompt. Its six `SessionStart` commands run `charliebot compaction-context --session <sid> --slot <1-6>`
    after compaction to restore the session's current read skills. The per-run-kind settings also disable connector
    sync and synced Claude.ai skills.
- **stdin:** the prompt (`_stdin_prompt`), written and closed by `_write_stdin_prompt`.
- **env:** `_prepare_env` (headless variables, `CLAUDE_CONFIG_DIR` of the pool account).
- **cwd:** common rule. A manager turn gets `CLAUDE.md` in its session directory.
- **Output:** Claude Code `stream-json` NDJSON on stdout. `translate_event` is the base identity
  (`[event]`): the server's event schema is this format.
- **Other `claude` processes:**
  - `one_shot_text`: `claude -p --output-format text --no-session-persistence --model <m> --system-prompt <s>
    --disallowed-tools Bash,Read,Write,Edit,Glob,Grep,Agent`, prompt on stdin, plain-text stdout.
  - Compaction (`claude_compaction.py::compaction_command`): `claude -p --resume <id> --model claude-sonnet-5
    --output-format json --disallowed-tools <...> --settings <compact JSON>`, with
    `"/compact\n"` on stdin and a JSON result on stdout. Its settings include the same six compact-matcher hooks
    for the CharlieBot session. Its env is `compaction_env` (`env.md` section 5).
  - Idle compaction (`claude_idle_compaction.py::_compaction_process`): a qualifying `cc-claude` master session
    runs its own configured model and account with the normal run-kind settings and environment, plus
    `--resume <id> --exclude-dynamic-system-prompt-sections`; it sends `"/compact\n"` through the same compaction
    runner. The trigger checks the idle context floor, transcript, backend, archive and turn state (`services.md` section 7).
  - Pool relays (`claude_relay.py`, `master_cc_relay.py`) move a transcript to another login with
    `claude_accounts.move_transcript` and resume there. **UNVERIFIED:** the relay argv; it is assumed to be the
    same `ClaudeCodeBackend` command with `--resume`, and the relay modules were not read.

### `src/backends/anthropic_compatible` (type `cc-anthropic-compatible`)

- `factory.py::build` makes a `claude_code.AnthropicEndpointBackend` with `base_url=api_base` (trailing `/` removed),
  the bearer token from `credentials.<credential>.api_key` (`no-auth` when `credential` is null), the option's
  `context_window`, and optional `headers`. Non-empty headers become newline-joined `name: value` entries in
  `ANTHROPIC_CUSTOM_HEADERS`. It is the same `claude` command, but without `--model` (the model rides env). Output is
  as cc-claude. The context limits come from `usage.py::claude_reading_limits`: the declared window and the cc-claude
  compaction reserves.

### `src/backends/openai_compatible` (type `cc-openai-compatible`)

- `openai_compatible_claude.py::OpenAICompatibleClaudeBackend`: the same `claude` command, with
  `ANTHROPIC_BASE_URL=http://localhost:<port>/api/anthropic-proxy/openai-compatible/<option id>` and
  `ANTHROPIC_AUTH_TOKEN=<charliebot.access_key>`.
- The server's own proxy (`anthropic_proxy.py`, a route owned by the HTTP contract) translates Anthropic Messages
  calls to OpenAI chat completions at `option.api_base`, authenticated with `credentials.<credential>.api_key`. A
  Rust server must serve that route for this backend type to work. Output is as cc-claude.

### `src/backends/codex` (type `codex`)

- **Executable:** `codex` from `PATH`, else `/home/<user>/.local/bin/codex` (`base.resolve_binary`). A missing
  binary raises `FileNotFoundError("codex binary not found on PATH or at <path>")`.
- **argv** (`codex.py::CodexBackend._build_command`):
  - `codex exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox --model <model>
    --config model_reasoning_effort="<effort, default xhigh>"`;
  - `[--config model_auto_compact_token_limit=<n>]`;
  - for a worker or review run, `--config developer_instructions=<TOML basic string of the instructions>` and
    `--config project_doc_fallback_filenames=["CLAUDE.md"]`;
  - then the extra flags, `--`, and the prompt;
  - a resume inserts `resume` after `exec` and appends the session id after the flags.
- **stdin:** none. **env:** `/home/<user>/.local/bin` on `PATH`. **cwd:** common rule; a manager turn gets
  `AGENTS.md`.
- **Output:** `codex exec --json` NDJSON with `thread.started`, `turn.started`, `turn.completed`, `turn.failed`,
  `error`, and `item.started` / `item.updated` / `item.completed`. Item types handled: `agent_message`,
  `reasoning`, `command_execution` (shown as `Bash`), `web_search`, `file_change`, `mcp_tool_call`, `todo_list`,
  `error`. Agent text is cumulative per item id; the translator emits deltas.
- Usage and quota read Codex rollouts under `/home/<user>/.codex/sessions` (`codex_usage.py`, `usage_quota.py`).

### `src/backends/gemini` (type `gemini`)

- **Executable:** `gemini` from `PATH`, else `/home/<user>/.local/bin/gemini`.
- **argv** (`gemini_cli.py::GeminiCliBackend._build_command`): `gemini -m <model> -p <prompt with the instructions
  framed in> -o stream-json -y --sandbox=false [--resume <id>] <extra flags>`. Instructions are framed into the
  prompt as `<system-instructions>...</system-instructions>`.
- **stdin:** none. **env:** `GEMINI_API_KEY` and `GOOGLE_API_KEY` removed.
- **Output:** `stream-json` NDJSON with types `init`, `message`, `tool_use`, `tool_result`, `error`, `result`.
  Assistant text is buffered and flushed as text events.

### `src/backends/antigravity` (type `antigravity`)

- **Executable:** `agy` from `PATH`, else `/home/<user>/.local/bin/agy`.
- **argv** (`antigravity_cli.py::AntigravityCliBackend._build_command`): `agy --print=<prompt with framed
  instructions> --print-timeout <print_timeout or 1h> --dangerously-skip-permissions --output-format json --model
  <model> [--conversation <id>] <extra flags>`.
- **stdin:** `/dev/null`. **env:** Google API keys removed; `/home/<user>/.local/bin` on `PATH`.
- **Spawn:** the piped variant with pdeathsig. stdout is teed to `<run dir>/stdout.log` (truncated per run) and
  stderr to `stderr.log`.
- **Output:** one JSON envelope on stdout at the end, an object with a string `status`, parsed by
  `_parse_envelope`. Fields read:
  - `status`: `SUCCESS`, or anything else for an error whose message comes from `error`, else `response`;
  - `conversation_id`: becomes `session_attached`;
  - `response`: the assistant text, with `<SYSTEM_MESSAGE>...</SYSTEM_MESSAGE>` blocks stripped;
  - `usage`: `input_tokens`, `output_tokens`, `thinking_tokens` (added to output), `cache_read_tokens`.
  A stderr line `[agy] print timeout after <d> with turn in progress` marks a turn timeout.

### `src/backends/charlie_code` (type `charlie-code`)

- **Executable:** `charlie-code` from `PATH`, else `/home/<user>/.local/bin/charlie-code`.
- **argv** (`charlie_code.py::CharlieCodeBackend._build_command`):
  - `charlie-code --json --model <model> --api-base <api_base>`;
  - then `[--context-window N]`, `[--no-stream]`, `[--timeout-seconds N]`, `[--top-p X]`, `[--temperature X]`,
    `[--resume <id>]`, `[--instructions-file <run dir>/harness_instructions.md]`, the extra flags, and
    `[--image <path>]` for each uploaded image;
  - then `--task-file <run dir>/task.md`. The task text is written to that file and never rides argv.
  - With `image_input: false`, image uploads are refused with one error event and nothing is spawned.
- **stdin:** none. **env:** `/home/<user>/.local/bin` on `PATH`, `CHARLIE_CODE_API_KEY`, proxy variables.
- **cwd:** common rule; a manager turn gets `AGENTS.md`.
- **Output:** NDJSON with types `session` (→ `session_attached`), `thought`, `command`, `command_progress`,
  `observation`, `result`, `error`, `compact`, `context` (→ a `context_reading` event).

### `src/backends/opencode` (type `opencode`)

- **Shared server** (`server.py::OpenCodeServer`), one per `proxy_url`:
  - executable `/home/<user>/.opencode/bin/opencode` (fixed path; a missing file raises `"opencode 2.0 binary not
    found at <path>"`); argv `opencode serve --port 0`; cwd `<home>/opencode`; stdin `/dev/null`; piped with
    pdeathsig; env per `env.md` section 5.
  - The listening URL is parsed from `server listening on (http://\S+)` in its output. The version must be 2.x.
  - HTTP basic auth uses user `opencode` and the random password.
  - Each turn claims its working directory until turn-end cleanup. When a claim drops, the server releases every
    loaded directory with no claim through `GET /api/debug/location` and `DELETE /api/debug/location`.
  - With no turn or claim for `OPENCODE_SERVER_HEAVY_CHECK_SECONDS` (60 s), it reads its own `/proc/<pid>/status`
    RSS; above `OPENCODE_SERVER_HEAVY_RSS_MB` (400 MiB) it stops. Otherwise it stops after
    `OPENCODE_SERVER_IDLE_EXIT_SECONDS` (3600 s) with no turn or claim. Child-process RSS is not included.
  - `OPENCODE_DISABLE_FFF=1` disables the file index.
- **Turn** (`opencode.py::OpenCodeBackend.run`):
  1. Spawn the turn's identity process `/usr/bin/sleep infinity` (piped, pdeathsig, joined to the session cgroup),
     then claim its working directory on the shared server.
  2. `GET /api/session/<id>` to resume, or `POST /api/session {location: {directory: cwd}, model: {id,
     providerID}}`. The option's `model` is `<provider>/<model>`. A recorded id missing from the store fails the
     turn.
  3. `GET /api/model?location[directory]=<cwd>` for the context limit.
  4. `PUT /api/session/<id>/environment {variables}` (`env.md` section 4.5).
  5. A manager turn calls `POST /api/location/reload` after rewriting `AGENTS.md`.
  6. `POST /api/session/<id>/prompt {text, files?}`.
  Turn-end cleanup finishes the turn, drops its directory claim and releases all loaded unclaimed directories.
  Cancelling the turn means signalling the sleep process.
- **Output:** the global SSE stream `GET /api/event`, lines `data: <json>`. Types used: `server.connected`,
  `session.execution.started`, `session.execution.interrupted`, `session.step.started`, `session.step.ended`,
  `session.text.delta`, `session.text.ended`, `session.reasoning.delta`, `session.reasoning.ended`,
  `session.tool.input.started`, `session.tool.called`, `session.tool.success`, `session.tool.failed`,
  `session.compaction.started`, `session.compaction.ended`. An execution no turn claims is interrupted.
- **Headless config** (`OPENCODE_CONFIG_CONTENT`):
  `{"permission": {"*": "allow", "question": "deny", "subagent": "deny", "opencode_session_move": "deny"},
  "shell": "bash", "default_agent": "charliebot", "agent": {"charliebot": {"mode": "primary"}}}`, plus a `providers`
  block with each `backends.options` entry's `context_limit` and fixed `extra_body` fields under the model's `body`
  (`src/backends/opencode/server.py:60-85`, `src/backends/opencode/options.py:19` `model_overrides_config`, `:35-37`).
- **One-shot:** `opencode run --standalone --format json -m <model> -- <framed prompt>` in a throwaway XDG root
  with every tool denied. Its NDJSON carries `text` and `error` events.

## 3. Other subprocesses the server starts

| Process | argv | Module |
| --- | --- | --- |
| git | `git rev-parse --short HEAD`, `git worktree add/remove --force/prune`, branch and merge operations | `src/infra/buildinfo.py`, `src/infra/git.py` |
| ssh | every argv comes from `src/infra/ssh.py::ssh_cmd`: `ssh -o BatchMode=yes -o ConnectTimeout=10 -o ControlMaster=auto -o ControlPath=/home/<user>/.ssh/controlmasters/%C -o ControlPersist=1200 <host> <remote argv>`. Remote argv: `true` (host_auth probe), `kill -0`, exit-file reads and `sacct` (triggers), and the `bash -c` launch wrapper (`cli/launcher.py`) | `src/features/host_auth/host_auth.py`, `src/infra/ssh.py`, `src/runtime/triggers.py`, `src/runtime/cli/launcher.py` |
| sacct | local `sacct` for `slurm_job` watches every 30 s | `src/runtime/triggers.py` |
| tmux | `tmux -L charliebot ...`; sessions `charliebot-<id>`; the terminal's id is `terminal` for the default home, else `terminal-<sha256(home)[:8]>` | `src/runtime/agent_process/pty_common.py`, `src/features/terminal/terminal.py` |
| code-server | `<code_server.bin> --config <path>`, detached, with stdio on `/dev/null` | `src/features/code_server/api.py` |
| node | `node <prerender driver> <fragment> <katex.min.js>` | `src/features/artifacts/artifact_wrap.py` |
| headless Chromium | `headless_chrome_bin` on a probe page | `src/features/artifacts/headless_render.py` |
| Perfetto merge helper | Python in a `ProcessPoolExecutor`, `PYTHONPATH=<repo>` | `src/features/trace/trace_merge.py` |
| launch wrapper (CLI side) | `bash -lc "(<cmd>\n); echo $? > <exit>"`, locally or over ssh | `src/runtime/cli/launcher.py` |
