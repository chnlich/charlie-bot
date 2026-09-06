# CharlieBot Project Specification

## 1. Project Overview & Objectives
**CharlieBot** is a Python-based system designed to coordinate and manage multiple **Claude Code** instances (Workers) to complete complex tasks. The primary interface is a responsive Web UI for desktop and mobile access.

### 1.1 Objectives
- **Agent Orchestration**: A **Master Agent** (running as a Claude Code session) manages and coordinates Worker instances via a CLI delegation workflow.
- **Task Delegation**: Master delegates coding tasks to Workers that run in isolated git worktrees; a separate Review Agent automatically verifies and merges the work.
- **Python-Native**: Built entirely in Python for extensibility and ease of integration with AI toolsets.

---

## 2. Technical Stack
- **Language**: Python 3.12+
- **Master Agent**: Claude Code session (pluggable backends)
- **Worker Agent**: Claude Code (local CLI invocation, non-interactive mode)
- **Backend**: FastAPI, WebSockets/SSE for real-time streaming, asyncio for concurrency
- **Frontend**: vanilla JS + Tailwind CSS, served by FastAPI StaticFiles
- **Storage**: JSON for state/data, YAML for configuration

---

## 3. Directory Structure

### 3.1 Home Directory (`~/.charliebot/` or `CHARLIEBOT_HOME`)
All instance-specific data (configs, sessions, memory) is stored here.

`CHARLIEBOT_HOME` selects which one: unset gives `~/.charliebot`, and a set value (absolute
or `~`-prefixed; relative is rejected) gives a separate profile, seeded on first use. Several
profiles run side by side on one host, each with its own port in its own `config.yaml`. The
home path is resolved in exactly one place, `charliebot_home_dir()` in `src/core/config.py`;
every other path derives from `CharlieBotConfig.charliebot_home`. One raw read of the variable
sits outside it: the web terminal's profile check (`src/agents/backends/terminal.py`). A tmux
pane inherits the tmux server's environment rather than the server process's, so the terminal
checks whether a profile is set and passes the resolved home to new panes explicitly.

```text
~/.charliebot/
├── config.yaml          # API keys, settings, and workspace_dirs list
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
- Individual Worker logs are in `threads/{uuid}/data/` (`stdout.log`, `stderr.log`, `events.jsonl`).
- `CLAUDE.md`: Written into each thread's worktree (so Claude Code finds it via cwd).
- `workspace_dirs`: Config option (`config.yaml`) listing workspace directories to scan for git projects. The `GET /api/sessions/projects` endpoint returns discovered projects for the UI project picker.

### 3.2 Repository Code Structure (Stateless)
```text
charlie-bot/
├── src/                # Python backend (api/, core/, agents/)
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
| **Master Agent** | Claude Code session (`src/agents/master_cc.py`) | User interaction, high-level planning, delegating coding tasks to Workers, reviewing combined worker+reviewer results. Runs as a persistent Claude Code subprocess with `--resume` support across messages. Can use any configured backend. |
| **Worker Agent** | Claude Code CLI (`src/agents/worker.py`) | Code analysis, implementation, file editing, git operations, testing. Runs in an isolated git worktree on a dedicated branch. Told NOT to rebase/merge/remove the worktree — a reviewer handles that. |
| **Review Agent** | Claude Code CLI (same Worker class) | Automatically spawned after a Worker succeeds. Reviews the diff, fixes issues, rebases onto base branch, merges (ff-only), and cleans up the worktree. Intentionally uses a DIFFERENT backend than the Worker (cross-backend review via `model_preference` config). |

**Backend Abstraction**: Workers and Master use a pluggable `AgentBackend` interface (`src/agents/backends/base.py`). The `BackendType` vocabulary (`src/core/models.py`) names the backends, and `src/agents/backends/registry.py` dispatches each `BackendOption.type` to its implementation. Backend selection is configured via `backend_options` and `model_preference` in `config.yaml`.

### 4.2 Session & Thread Model
- **Session**: Represents a project/workspace. Each Session has:
  - A `cc_session_id` for resuming the Master Agent's Claude Code conversation
  - Multiple Threads (concurrent Workers and Reviewers)

- **Thread**: Represents a single Worker or Reviewer task. Each Thread has:
  - Its own isolated Git branch (e.g., `charliebot/task-{timestamp}-{id}`)
  - A dedicated worktree directory
  - Metadata fields: `branch_name`, `repo_path`, `worktree_path`, `backend`, `model`, `context`
  - Reviewer-specific fields: `review_of` (links to original worker thread), `tried_backends` (for retry tracking)

### 4.3 Git Isolation Strategy
- **Thread Branch Isolation**: Each Worker operates on its own branch in an isolated git worktree to prevent conflicts
- **Reviewer Merge**: The review agent rebases the worker's branch onto the base branch and merges with `--ff-only`
- **Worktree Cleanup**: The reviewer removes the worktree after successful merge

---

## 5. Core Workflows

### 5.1 Delegation Workflow
The Master Agent delegates coding tasks to Workers via the CLI delegate command:

1. **Task Delegation** (Master → Worker):
   - User submits request via Web UI chat
   - Master Agent (Claude Code session) decides to delegate a coding task
   - Master calls `charliebot delegate --repo /path --base-branch main --task-spec-file <file>` (`src/cli/delegate.py`); session identity comes from the `CHARLIEBOT_SESSION_ID` the server writes into the master process, with cwd as the fallback when it is absent
   - The CLI POSTs to `/api/internal/delegate`, which creates a thread and spawns a Worker via `spawn_worker()` (`src/core/spawner.py`)

2. **Worker Execution** (Phase 1 — Implement):
   - Spawner creates an isolated git worktree on a new branch (`charliebot/task-{ts}-{id}`)
   - Worker receives a prompt with session info, worktree instructions, and the task description
   - Worker is explicitly told: "Do NOT rebase, merge, or remove the worktree. A reviewer will handle that."
   - Worker implements the task, commits changes, and exits
   - Events are streamed via WebSocket and persisted to `events.jsonl`

3. **Review** (Phase 2 — Automatic on Worker Success):
   - On successful worker completion, `spawn_review_worker()` (`src/core/review.py`) automatically spawns a Review Agent
   - The reviewer intentionally uses a **different LLM backend** than the worker (cross-backend review), selected from `model_preference` config
   - Reviewer reads session conversation + worker log for context, then:
     - Reviews `git diff base_branch...branch_name`
     - Fixes any issues found, commits fixes
     - Rebases the branch onto the base branch
     - Merges back to main with `--ff-only`
     - Cleans up the worktree
   - If the reviewer **fails**, it retries with the next untried backend from `model_preference`. Max retries = `len(model_preference)`

4. **Master Trigger on Completion**:
   - After review completes (success or all retries exhausted), the master agent is triggered via `trigger_master()` with a combined summary of worker + reviewer results
   - If the worker itself failed (no review spawned), the master is triggered immediately with the worker's summary
   - The master can then inform the user and decide on follow-up actions

5. **Thread Metadata Tracking**:
   - `review_of`: Links a reviewer thread to its original worker thread
   - `tried_backends`: Tracks which backends have been attempted for reviewer retries
   - `branch_name`, `worktree_path`, `repo_path`: Git isolation state
   - `backend`, `model`: Which LLM backend/model was used

### 5.2 Plan Mode (Two-Phase Execution)
For complex tasks, CharlieBot uses a planning phase:

**Phase 1: Planning**
- Master Agent determines task requires planning phase
- A "Plan Thread" is created with `--plan-mode` flag
- Worker analyzes and outputs detailed execution plan (no file modifications)
- Plan displayed in Web UI as editable checklist

**Phase 2: Execution**
- User reviews, edits, or approves the plan
- Approved steps are delegated as individual worker tasks
- Multiple plans can execute in parallel across different Threads

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
- **Worker Layer**: Task decomposition; file scoping via `CLAUDE.md` (explicitly limit focus to relevant modules)

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
4. **Local sherpa-onnx Qwen3-ASR** transcribes (VAD + simulated-streaming partials; supports Chinese, English, mixed, and ~30 languages)
5. Transcription displayed in UI first
6. Passed to Master with disclaimer: *"This is a voice-transcribed message and may not be exactly accurate. Please ask clarifying questions if anything is unclear."*

---

## 8. Communication & Monitoring

### 8.1 Real-Time Streaming
- **WebSockets or SSE**: Stream PTY output directly from Worker to frontend
- **HTTP GET**: Used for loading historical logs (non-real-time)
- **Persistence**: Worker state is flushed to disk in real-time; Master can resume after restart

### 8.2 JSON Stream Monitoring
Workers run with `--output-format stream-json --verbose`:
```json
{"type": "thinking", "content": "Analyzing..."}
{"type": "file_write", "path": "src/auth.py", "lines_added": 45}
{"type": "error", "message": "ImportError..."}
{"type": "complete", "status": "success"}
```
Master parses this to distinguish "thinking" from "stuck" and track progress precisely.

---

## 9. Error Handling & Resilience

### 9.1 Model Fallback
- Multiple backends configured via `backend_options` in `config.yaml` (see that file for the current list)
- `model_preference` controls reviewer backend selection order, enabling cross-backend code review
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
  (`[tool.ruff.lint]` in `pyproject.toml`, CI runs `uv run ruff check src`):
  ```ini
  [style]
  based_on_style = google
  indent_width = 2
  split_before_first_argument = true
  column_limit = 120
  ```
  The code-health cron runs `yapf --in-place --recursive src/` and verifies with
  `yapf --diff`, so the config file is load-bearing: without it YAPF falls back to
  pep8 defaults and reformats the tree to 4-space indent.

### 10.2 Worker Instructions (CLAUDE.md)
Each Thread's `CLAUDE.md` contains:
1. Default shared instructions from `~/.charliebot/SUBAGENT_PROMPT.md` (coding standards, YOLO mode, git conventions)
2. Specific task description and objectives
3. Session-specific context/constraints

---

## 11. Implementation Status

### 11.1 Completed (MVP)

**Backend**
- FastAPI server (`server.py`)
- All API routes: `/api/sessions`, `/api/chat`, `/api/threads`, `/api/internal/delegate` (full list: the `include_router` calls in `server.py`)
- Master Agent as Claude Code session (`src/agents/master_cc.py`) with `--resume` support for persistent conversations. Supports any configured backend via the pluggable `AgentBackend` interface
- Delegation CLI (`src/cli/delegate.py`) — called by the master to spawn workers via `POST /api/internal/delegate`
- Worker spawner (`src/core/spawner.py`) — creates isolated git worktrees, builds enriched prompts, spawns workers, and orchestrates the two-phase worker+reviewer pipeline
- Automatic cross-backend review: on worker success, a Review Agent is spawned using a different LLM backend (configurable via `model_preference`). Failed reviewers retry with the next untried backend
- Master trigger on completion: combined worker+reviewer summary is sent to the master agent via `trigger_master()` for user notification and follow-up decisions
- `SessionManager`, `ThreadManager`, `PlanRegistryManager`, `TriggerManager`, `StreamingManager`
- `init_charliebot_home()` — seeds `~/.charliebot/` on first run with default `config.yaml` and the memory store scaffold (git repo + topics vocabulary)
- Memory updates: sessions stage candidates via `charliebot memory add` (writes `staging/`, never `entries/`); the daily memory curator builds a user-approved diff that admits, revises, or evicts entries

**WebSocket Endpoints**
- `/ws/sessions/{session_id}` — session-level events (worker completion summaries pushed to chat)

**Frontend**
- Vanilla-JS UI under `web/static/js/`, served by FastAPI StaticFiles (Node.js/npm is build-time only: Tailwind CSS)
- Panels: Sessions sidebar, Chat (SSE streaming), Threads list, Plan review checklist, Voice push-to-talk
- ChatPanel subscribes to session WebSocket — receives worker summaries and renders them as assistant messages
- ThreadsPanel polls every 3 seconds for thread status updates
- No-cache middleware on HTML to prevent stale JS bundles
- Draft persistence: unsent message text is saved to localStorage per session (debounced 300ms) and restored on session switch-back or page reload

**Configuration**
- `~/.charliebot/config.yaml` is the single source of truth for API keys and settings — no environment variables
- `backend_options`: configurable list of LLM backends (see that file for the current list)
- `model_preference`: ordered list of backend IDs for cross-backend reviewer selection

### 11.2 Pending / Not Yet Implemented

- Claude plan usage tracking and automatic fallback for workers when near quota limits
