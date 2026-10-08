# CharlieBot Project Specification

## 1. Project Overview & Objectives
**CharlieBot** is a Python-based system designed to coordinate and manage multiple **Claude Code** instances (Workers) to complete complex tasks. The primary interface is a responsive Web UI for desktop and mobile access.

### 1.1 Objectives
- **Agent Orchestration**: A **Master Agent** (running as a Claude Code session) manages and coordinates Worker instances via a CLI delegation workflow.
- **Task Delegation**: Master delegates coding tasks to Workers that run in isolated git worktrees; a separate Review Agent automatically verifies and merges the work.
- **Python-Native**: Built entirely in Python for extensibility and ease of integration with AI toolsets.

---

## 2. Technical Stack
- **Language**: Python 3.14
- **Master Agent**: Claude Code session (pluggable backends)
- **Worker Agent**: Claude Code (local CLI invocation, non-interactive mode)
- **Backend**: FastAPI, WebSockets for real-time streaming, asyncio for concurrency
- **Frontend**: vanilla JS + Tailwind CSS, served by FastAPI StaticFiles
- **Storage**: JSON for state/data, YAML for configuration

---

## 3. Directory Structure

### 3.1 Home Directory (`~/.charliebot/` or `CHARLIEBOT_HOME`)
All instance-specific data (configs, sessions, memory) is stored here.

`CHARLIEBOT_HOME` selects which one: unset gives `~/.charliebot`, and a set value (absolute
or `~`-prefixed; relative is rejected) gives a separate profile, seeded on first use. Several
profiles run side by side on one host, each with its own port in its own `config.yaml`. The
home path is resolved in exactly one place, `charliebot_home_dir()` in `src/infra/home.py`;
every other path derives from `CharlieBotConfig.charliebot_home`. One raw read of the variable
sits outside it: the web terminal's profile check (`src/features/terminal/terminal.py`). A tmux
pane inherits the tmux server's environment rather than the server process's, so the terminal
checks whether a profile is set and passes the resolved home to new panes explicitly.

```text
~/.charliebot/
├── config.yaml          # Structure settings in sections; workspace_dirs list
├── credentials.yaml     # Secrets (API keys, tokens), section → key
├── memory/             # Labeled-entry memory store (local git repo)
│   ├── entries/<topic>/<slug>.md   # canonical entries (one fact per file)
│   ├── topics                       # controlled topic vocabulary
│   └── staging/                     # candidate entries (.gitignore'd)
└── sessions/            # Session directories
    └── {session_uuid}/
        ├── metadata.json      # Session info (name, status, timestamps)
        ├── data/              # Session-level JSON data
        └── threads/           # Thread directories
            └── {thread_uuid}/
                ├── metadata.json    # Thread info (task description, status)
                └── data/            # Thread-specific JSON data (logs, state)
```

**Worktrees** are stored under `worktree_dir` (config, default `~/worktrees`), one directory per
thread branch — the directory name is the branch name with `/` replaced by `-`:

```text
<worktree_dir>/
└── charliebot-task-{ts}-{id}/      # Thread worktree (isolated branch `charliebot/task-{ts}-{id}`)
```

**Notes:**
- Individual Worker logs are in `threads/{uuid}/data/`: the raw-log spawn writes `agent.raw.ndjson` (the
  CLI's stream) plus `agent.stderr.log`; the pipe transports write `stdout.log` plus `stderr.log`;
  `events.jsonl` holds the translated events.
- `workspace_dirs`: Config option (`config.yaml`) listing workspace directories to scan for git projects. The `GET /api/sessions/projects` endpoint returns discovered projects for the UI project picker.

### 3.2 Repository Code Structure (Stateless)
```text
charlie-bot/
├── src/                # Python backend (infra/, runtime/, backends/, features/, app/)
├── web/                # Web UI (templates/ + static/)
├── configs/            # Default templates and examples
├── server.py           # Entry point
└── project.md          # This specification
```

---

## 4. Core Architecture

### 4.1 Agent Roles
| Role | Type | Responsibilities |
|------|------|------------------|
| **Master Agent** | Claude Code session (`src/runtime/master_cc.py`) | User interaction, high-level planning, delegating coding tasks to Workers, reviewing combined worker+reviewer results. Runs as a persistent Claude Code subprocess with `--resume` support across messages. Can use any configured backend. |
| **Worker Agent** | Claude Code CLI (`src/runtime/worker.py`) | Code analysis, implementation, file editing, git operations, testing. Runs in an isolated git worktree on a dedicated branch. Told NOT to rebase/merge/remove the worktree — a reviewer handles that. |
| **Review Agent** | Claude Code CLI (same Worker class) | Automatically spawned after a Worker succeeds. Reviews the diff, fixes issues, rebases onto the remote base, and pushes the branch to the base (git rejects a non-fast-forward push). Intentionally uses a DIFFERENT backend than the Worker (cross-backend review via `backends.preference` config). |

**Backend Abstraction**: Workers and Master use a pluggable `AgentBackend` interface (`src/runtime/agent_process/base.py`). Each backend package registers its type string, its option model and its factory with `src/runtime/hooks/backend_types.py` (`src/app/registrations.py` lists the packages), which dispatches each `BackendOption.type` to its implementation; config loading parses each `backends.options[]` entry through the option models that `src/infra/config_registry.py` holds. Backend selection is configured via `backends.options` and `backends.preference` in `config.yaml`.

### 4.2 Session & Thread Model
- **Session**: Represents a project/workspace. Each Session has:
  - A `cc_session_id` for resuming the Master Agent's Claude Code conversation
  - Multiple Threads (concurrent Workers and Reviewers)

- **Thread**: Represents a single Worker or Reviewer task. Each Thread has:
  - Its own isolated Git branch (e.g., `charliebot/task-{timestamp}-{id}`)
  - A dedicated worktree directory
  - Metadata fields: `branch_name`, `repo_path`, `worktree_path`, `backend`, `model`, `context`
  - Reviewer-specific field: `tried_backends` (for retry tracking)

### 4.3 Git Isolation Strategy
- **Thread Branch Isolation**: Each Worker operates on its own branch in an isolated git worktree to prevent conflicts
- **Reviewer Merge-Back**: The review agent rebases the worker's branch onto the remote base branch and pushes it there; git rejects a non-fast-forward push, so the base only fast-forwards
- **Worktree Cleanup**: The executor removes the worktree once the task actually delivers; failed, blocked and unproven outcomes keep it, and `keep_worktree` pins it

---

## 5. Core Workflows

### 5.1 Delegation Workflow
The Master Agent delegates coding tasks to Workers via the CLI delegate command:

1. **Task Delegation** (Master → Worker):
   - User submits request via Web UI chat
   - Master Agent (Claude Code session) decides to delegate a coding task
   - Master calls `charliebot delegate --repo /path --base-branch main --task-spec-file <file>` (`src/runtime/cli/delegate.py`); session identity comes from the `CHARLIEBOT_SESSION_ID` the server writes into the master process, with cwd as the fallback when it is absent
   - The CLI POSTs to `/api/internal/delegate`, which creates the worker task-tree child and registers its first work Run (`_delegate_task_tree` in `src/runtime/api/internal.py`); the tree's executor launches it (`execute_run` in `src/runtime/task_execution.py`)

2. **Worker Execution** (Phase 1 — Implement):
   - Spawner creates an isolated git worktree on a new branch (`charliebot/task-{ts}-{id}`)
   - Worker receives a prompt with session info, worktree instructions, and the task description
   - Worker is explicitly told: "Do NOT rebase, merge, or remove the worktree. A reviewer will handle that."
   - Worker implements the task, commits changes, and exits
   - Events are streamed via WebSocket and persisted to `events.jsonl`

3. **Review** (Phase 2 — Automatic on Worker Success):
   - On successful work-run completion, `_maybe_spawn_review` (`src/runtime/task_execution.py`) registers a review Run on the same task and launches it, picking the reviewer backend with `select_reviewer_backend` (`src/runtime/review.py`)
   - The reviewer intentionally uses a **different LLM backend** than the worker (cross-backend review), selected from `backends.preference` config
   - Reviewer reads session conversation + worker log for context, then:
     - Reviews `git diff base_branch...branch_name`
     - Fixes any issues found, commits fixes
     - Rebases the branch onto the base branch
     - Pushes the rebased branch to the remote base (`git push origin HEAD:{base_branch}`); git rejects a non-fast-forward push
     - Cleans up the worktree
   - If the reviewer **fails**, it retries with the next untried backend from `backends.preference`. Max retries = `len(backends.preference)`

4. **Master Trigger on Completion**:
   - After review completes (success or all retries exhausted), the master agent is triggered via `trigger_master()` with a combined summary of worker + reviewer results
   - If the worker itself failed (no review spawned), the master is triggered immediately with the worker's summary
   - The master can then inform the user and decide on follow-up actions

5. **Thread Metadata Tracking**:
   - `tried_backends`: Tracks which backends have been attempted for reviewer retries
   - `branch_name`, `worktree_path`, `repo_path`: Git isolation state
   - `backend`, `model`: Which LLM backend/model was used

### 5.2 Plan Registry (Draft, Approve, Delegate)
For complex tasks, the master plans before building; the plan registry keeps that lifecycle:

- **Draft & present**: The master drafts the plan as an HTML artifact (`artifacts/plan_NN.html`, grammar in `prompts/plan_template.html`) and registers it with `charliebot plan present --file <artifact> --title <title>` (`src/features/artifacts/plan_cli.py` → `PlanRegistryManager` in `src/features/artifacts/plans.py`). `charliebot plan amend --file <artifact> --note <why>` appends the next version (trigger: `auto_amend` or `feedback`).
- **Review**: The plan renders in the web Plans panel with a version switcher, a diff toggle against the predecessor, and block-anchored comments (`web/static/js/plan-panel.js`).
- **Approve**: The user's "take off" approves the settled terms; `charliebot plan approve` records it against the latest version. The takeoff gate (`src/runtime/takeoff_gate.py`) lets `/delegate` and `/improve` proceed only when the session's latest real user message carries the approval (or a "pre take off" stamp within 12 hours).
- **Close**: `charliebot plan close --plan N --as superseded|abandoned|completed` terminates the lineage.

---

## 6. Memory & Knowledge Management

### 6.1 Labeled-Entry Memory Store
A local git repo at `~/.charliebot/memory/` holds one durable fact or rule set per file under
`entries/<topic>/<slug>.md`, tagged by `scope`/`topic`/`audience` in a restricted front matter.

| Path | Purpose | Access Pattern |
|------|---------|----------------|
| **memory/entries/** | Canonical entries (user preferences, host facts, master guidance) | Resident topics injected in full at master spawn; topic-matched entries at worker spawn; queryable mid-session via `charliebot memory query` |
| **memory/staging/** | Candidate entries proposed by sessions | Written by `charliebot memory add`; never auto-merged; admitted only via user-approved curation diffs |

### 6.2 Context Management Strategies
- **Master Layer**: Conversation summarization (compress early history, keep last ~10 turns); hierarchical context (System > Session Summary > Recent Dialogue > Retrieved snippets)
- **Worker Layer**: Task decomposition; file scoping via the task spec (explicitly limit focus to relevant modules)

---

## 7. User Interface

### 7.1 Web UI Layout
- **Sessions (Sidebar)**: Multi-channel organization (like Slack). Each session = separate project/context.
- **Chat Interface**: Main area for Master Agent interaction (ChatGPT-like).
- **Threads (Sub-Agents)**: Nested under sessions. Each thread = Claude Code Worker task. Users can drill down to view status/logs.

### 7.2 Voice Input (Push-to-Talk)
**Workflow**:
1. User presses/clicks button to start recording
2. Presses/clicks again to stop and send
3. Audio uploaded to backend
4. **Local speech transcription** decodes the complete recording offline: the VAD segments it and each segment decodes in one shot (sherpa-onnx Qwen3-ASR on CPU by default, `voice.engine=qwen3_hf` on GPU hosts; supports Chinese, English, mixed, and ~30 languages)
5. Transcription displayed in UI first
6. Passed to Master with a disclaimer prefix: the displayed message stays verbatim, and the prompt the agent receives carries the fixed voice note from `_VOICE_DISCLAIMER` (`src/runtime/master_cc_run.py`)

---

## 8. Communication & Monitoring

### 8.1 Real-Time Streaming
- **WebSockets**: Stream PTY output and live events directly to the frontend; the browser never consumes SSE (SSE parsing exists only server-side, for upstream LLM streams)
- **HTTP GET**: Used for loading historical logs (non-real-time)
- **Persistence**: Worker state is flushed to disk in real-time; Master can resume after restart

### 8.2 JSON Stream Monitoring
Workers run with `--output-format stream-json --verbose`, so the raw NDJSON log (`agent.raw.ndjson`)
holds the CLI's stream. The lines are `assistant` events carrying `message.content` blocks
(`text`, `thinking`, `tool_use`), `user` tool-result wrappers, and a final `result`.
`AgentBackend.translate_event` (`src/runtime/agent_process/base.py`) turns each line into the events that
land in `events.jsonl` and the WebSocket stream: the Anthropic-endpoint backends (cc-claude,
cc-kimi, cc-openai-compatible) pass lines through unchanged; the other backends translate their
native streams into CC-compatible events with per-backend vocabularies (codex, for one, emits
`thinking` and `file_write`).
A raw log that stops growing while the process is still alive is what the server reports as stuck
(the no-output silence report); thinking in progress is the `thinking` content.

---

## 9. Error Handling & Resilience

### 9.1 Model Fallback
- Multiple backends configured via `backends.options` in `config.yaml` (see that file for the current list)
- `backends.preference` controls reviewer backend selection order, enabling cross-backend code review
- Failed reviewers automatically retry with the next untried backend

### 9.2 Rebase Conflict Handling
- The Review Agent rebases the worker branch onto the base branch before merging
- If the rebase fails (conflicts), the reviewer attempts to resolve them as part of its review
- If unresolvable, the review fails and retries with the next backend

---

## 10. Development Guidelines

### 10.1 Code Style
- **Standard**: Google Code Style (2-space indent, 120 column limit)
- **Python**: formatted by YAPF (`.style.yapf`), lint-enforced by ruff
  (`[tool.ruff.lint]` in `pyproject.toml`, CI runs `uv run ruff check src tests`):
  ```ini
  [style]
  based_on_style = google
  indent_width = 2
  split_before_first_argument = true
  column_limit = 120
  ```
  `tools/check-google-style.sh` runs YAPF and the module-import check over the tree;
  the code-health cron takes its style cleanups from that probe's report. Without
  `.style.yapf`, a yapf run falls back to pep8 defaults and reformats the tree to
  4-space indent, which is why the CI "Formatter config present" step keeps the
  config and the pin in place.
- **Imports**: modules only (Google Python Style Guide 2.2), checked by
  `tools/check-google-style.sh` through the pylint-google-style plugin
  (`[tool.pylint]` in `pyproject.toml`).

### 10.2 Worker Instructions
Worker and reviewer directives (role, skills discovery, worktree workflow, coding standards) ride in the prompt
itself: `prompts/worker.md` sections assembled by `_build_worker_prompt` (`src/runtime/spawner_prompt.py`) for
workers, `review_rules_text` (`src/runtime/review.py`) for reviewers, rendered into the managed instructions by
`_worker_kind_rule_segments` (`src/runtime/task_prompts.py`) beside the volatile review context that
`_build_review_context` (`src/runtime/task_execution.py`) assembles. No instruction file is written into a
worker's worktree, so the checked-out repo's own AGENTS.md/CLAUDE.md stays in effect. Master sessions are the
only path that writes one: `_build_instructions_content` (`src/runtime/master_cc_run.py`) assembles the
git-shared base prompt, the per-host override, the memory block, and the project layer, and the backend writes
it to the session cwd (CLAUDE.md for Claude Code, AGENTS.md for the other backends).

---

## 11. Implementation Status

### 11.1 Completed (MVP)

**Backend**
- FastAPI server (`server.py`)
- All API routes: `/api/sessions`, `/api/chat`, `/api/threads`, `/api/internal/delegate` (full list: the `include_router` calls in `server.py` and the packages in `src/app/registrations.py`)
- Master Agent as Claude Code session (`src/runtime/master_cc.py`) with `--resume` support for persistent conversations. Supports any configured backend via the pluggable `AgentBackend` interface
- Delegation CLI (`src/runtime/cli/delegate.py`) — called by the master to spawn workers via `POST /api/internal/delegate`
- Worker spawner (`src/runtime/spawner.py`) — creates isolated git worktrees, builds enriched prompts, spawns workers, and orchestrates the two-phase worker+reviewer pipeline
- Automatic cross-backend review: on worker success, a Review Agent is spawned using a different LLM backend (configurable via `backends.preference`). Failed reviewers retry with the next untried backend
- Master trigger on completion: combined worker+reviewer summary is sent to the master agent via `trigger_master()` for user notification and follow-up decisions
- `SessionManager`, `ThreadManager`, `PlanRegistryManager`, `TriggerManager`, `StreamingManager`
- `init_charliebot_home()` — seeds `~/.charliebot/` on first run with default `config.yaml` and the memory store scaffold (git repo + topics vocabulary)
- Memory updates: sessions stage candidates via `charliebot memory add` (writes `staging/`, never `entries/`); the daily memory curator builds a user-approved diff that admits, revises, or evicts entries

**WebSocket Endpoints**
- `/ws/sessions/{session_id}` — session-level events (worker completion summaries pushed to chat)
- `/ws/terminal` — the profile's tmux-backed web terminal

Voice input records locally and ends one of two ways. With a live transcription backend the
browser also streams every audio chunk to the preview relay `/ws/voice/{session_id}`; on stop
the relay archives the recording it already received (under `sessions/{id}/voice/`, 16 kHz mono
PCM16 WAV plus a `.txt` with the final text) and pushes the final, and the browser uploads
nothing. The fallback — no final inside the 2 s budget, a relay failure, or the local backend —
uploads the whole recording: `POST /api/voice/{session_id}` persists it under
`sessions/{id}/voice/`, then decodes it offline, and `POST /api/voice/{session_id}/confirm`
decodes the opening clip as a recognition probe.

**Frontend**
- Vanilla-JS UI under `web/static/js/`, served by FastAPI StaticFiles (Node.js/npm is build-time only: Tailwind CSS)
- Panels: the sessions sidebar plus the tab strip — Terminal, Chat w/ TeX, Backlog, Plans, Chat
- The session WebSocket (`web/static/js/websocket.js`) drives the chat: rendering is fully driven by aggregated `message`/`stream` deltas, so a worker completion summary arrives as an assistant message
- Polls: the sidebar status poll adapts (3 s with running tasks, 10 s idle); an open session view polls its usage every 3 s while the master is thinking, and an open worker transcript polls every 2 s (all ride the page-timers registry, so a hidden tab polls nothing)
- Draft persistence: unsent message text is saved to localStorage per session (debounced 300ms) and restored on session switch-back or page reload

**Configuration**
- `~/.charliebot/config.yaml` holds structure in sections (`server`, `paths`, `backends`, `accounts`, `voice`, `code_server`, `ui`, `slack`, `publish`, `telegram`); `~/.charliebot/credentials.yaml` holds every secret as section → key and is the single source of truth for API keys — no environment variables
- `backends.options`: configurable list of LLM backends (see that file for the current list); an option id names the model family, never a version (the id rule: the `BackendsConfig` comment in `src/infra/config.py`)
- `backends.preference`: ordered list of backend IDs for cross-backend reviewer selection; server startup (`require_backends`) refuses a preference entry or cron task backend that names no option id
