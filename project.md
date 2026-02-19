# CharlieBot Project Specification

## 1. Project Overview & Objectives
**CharlieBot** is a Python-based system designed to coordinate and manage multiple **Claude Code** instances (Workers) to complete complex tasks. The primary interface is a responsive Web UI for desktop and mobile access.

### 1.1 Objectives
- **Agent Orchestration**: Enable an API-based **Master Agent** (Gemini 3 Flash, Kimi k2.5) to manage and coordinate **Claude Code** Worker instances.
- **Task Delegation**: Master delegates coding tasks to Workers; actual code analysis and implementation planning is performed by the Workers themselves.
- **Python-Native**: Built entirely in Python for extensibility and ease of integration with AI toolsets.

---

## 2. Technical Stack
- **Language**: Python 3.10+
- **Master Agent**: API-based LLM (Gemini 3 Flash primary, Kimi k2.5 fallback)
- **Worker Agent**: Claude Code (local CLI invocation, non-interactive mode)
- **Backend**: FastAPI (or Flask), WebSockets/SSE for real-time streaming, asyncio for concurrency
- **Frontend**: React SPA built to static files, served by FastAPI
- **Storage**: JSON for state/data, YAML for configuration

---

## 3. Directory Structure

### 3.1 Home Directory (`~/.charliebot/` or `CHARLIEBOT_HOME`)
All instance-specific data (configs, logs, sessions, worktrees) is stored here:

```text
~/.charliebot/
├── config.yaml      # API keys, settings, and project_dirs list
├── MEMORY.md        # Globally shared memory (user preferences, facts)
├── PAST_TASKS.md    # Global record of all completed tasks
├── PROGRESS.md      # Global lessons learned and insights
├── backups/         # Automatic hourly snapshots (7-day retention)
├── logs/            # Application logs (server errors, system events)
└── sessions/        # Session directories
    └── {session_uuid}/
        ├── metadata.json      # Session info (name, repo, branch)
        ├── task_queue.json    # Session's priority task queue
        ├── repo.git/          # Session-scoped bare git repository
        ├── worktree/          # Session's base Git worktree
        ├── data/              # Session-level JSON data
        └── threads/           # Thread directories
            └── {thread_uuid}/
                ├── metadata.json    # Thread info (task description, status)
                ├── worktree/        # Thread's Git worktree (isolated branch)
                ├── data/            # Thread-specific JSON data (logs, state)
                └── CLAUDE.md        # Worker's task instructions
```

**Notes:**
- `logs/`: Stores application-level logs (server errors, HTTP access, Worker spawn/crash events). Individual Worker logs are in `threads/{uuid}/data/`.
- `backups/`: Hourly snapshots of MEMORY.md, PAST_TASKS.md, Session metadata, and Thread data. 7-day retention with daily snapshots beyond.
- `repo.git/`: Bare git repository is now session-scoped (previously stored in global `~/.charliebot/repos/`). Legacy repos are lazily migrated on access.
- `project_dirs`: Config option (`config.yaml`) listing workspace directories to scan for git projects. The `GET /api/sessions/projects` endpoint returns discovered projects for the UI project picker.

### 3.2 Repository Code Structure (Stateless)
```text
charlie-bot/
├── src/                # Python backend (api/, core/, agents/)
├── web/                # React frontend (src/ for dev, static/ for runtime)
├── config/             # Default templates (claude-default.md, examples)
├── .yapf               # YAPF code style configuration
├── server.py           # Entry point
└── project.md          # This specification
```

---

## 4. Core Architecture

### 4.1 Agent Roles
| Role | Type | Responsibilities |
|------|------|------------------|
| **Master Agent** | API LLM (Gemini 3 Flash / Kimi k2.5) | User interaction, task classification (P0/P1/P2), high-level planning decisions, reviewing Worker results. Does NOT perform code analysis or queue operations. |
| **Queue Manager** | Python (Deterministic) | Hard-coded queue logic: push/pop/reorder tasks, monitor Worker slots, spawn Workers. Stateless, no LLM involvement. |
| **Worker Agent** | Claude Code CLI | Code analysis, implementation planning, file editing, git operations, testing. Runs in non-interactive mode (`claude -p --dangerously-skip-permissions`). |

**Model Abstraction**: The LLM client is wrapped in an abstract `LLMProvider` class, enabling easy addition of other models without modifying core logic.

### 4.2 Session & Thread Model
- **Session**: Represents a project/workspace. Each Session has:
  - One Git Worktree (associated with a specific repo/branch)
  - A priority task queue (`task_queue.json`)
  - Multiple Threads (concurrent Workers)
  
- **Thread**: Represents a single Worker task. Each Thread has:
  - Its own isolated Git branch (e.g., `charliebot/task-{timestamp}-{id}`)
  - A dedicated worktree directory
  - A `CLAUDE.md` file with task instructions

### 4.3 Git Isolation Strategy
- **Session Worktree**: Base working directory for the project
- **Thread Branch Isolation**: Each Worker operates on its own branch to prevent conflicts
- **Conflict Resolution**: When auto-merge fails, a dedicated **Conflict Resolution Worker** reads commit messages from both branches, analyzes the changes, and performs an intelligent merge

---

## 5. Core Workflows

### 5.1 Ralph Loop (Continuous Task Consumption)
CharlieBot operates in a continuous loop where Workers automatically pull and execute tasks:

1. **Task Queue** (`task_queue.json`): Three priority levels
   - **P0 (Immediate)**: Active conversation — execute immediately
   - **P1 (Standard)**: Features/bugs — consume when slots available
   - **P2 (Background)**: Refactoring/docs — batch during idle time

2. **Task Ingestion** (Master Agent - LLM):
   - User submits request via chat
   - Master Agent classifies priority (P0/P1/P2) and generates task description
   - Python Queue Manager receives the classified task and pushes to queue

3. **Execution Flow** (Queue Manager - Python):
   - Queue Manager (deterministic Python code) monitors available Worker slots
   - When slot free, Queue Manager pops highest priority task from queue
   - Queue Manager spawns Worker and passes task context
   - Worker executes and **automatically exits**
   - Queue Manager immediately triggers next iteration

4. **Completion Handling** (Master Agent - LLM):
   - When Worker finishes, Master Agent reviews results
   - Master summarizes to user and decides if follow-up tasks needed
   - If new tasks identified, Master classifies and submits to Queue Manager

5. **Survivability**: On restart, Queue Manager reloads queue state and resumes from where it left off

### 5.2 Plan Mode (Two-Phase Execution)
For complex tasks, CharlieBot uses a planning phase:

**Phase 1: Planning** (Master Agent decides, Queue Manager executes)
- Master Agent determines task requires planning phase
- Queue Manager creates "Plan Thread" with `--plan-mode` flag
- Worker analyzes and outputs detailed execution plan (no file modifications)
- Plan displayed in Web UI as editable checklist

**Phase 2: Execution** (User approves, Queue Manager schedules)
- User reviews, edits, or approves the plan
- Upon approval, Master Agent classifies plan steps as P0/P1
- Queue Manager pushes approved steps into task queue
- Execution Workers run approved steps (popped by Queue Manager)
- Multiple plans can execute in parallel across different Threads

---

## 6. Memory & Knowledge Management

### 6.1 Global Knowledge Files (Concurrency Guarded)
All files use synchronization locks to prevent race conditions:

| File | Purpose | Access Pattern |
|------|---------|----------------|
| **MEMORY.md** | User preferences ("dark mode"), facts ("works at Citadel"), personalization | Read at session start; updated when preferences/facts revealed |
| **PAST_TASKS.md** | Archive of all completed tasks (description, approach, files, solutions) | **On-demand only** — queried via semantic search when historical context needed |
| **PROGRESS.md** | Lessons learned, mistakes, best practices | Workers append insights at end of tasks ("summarize, refine, elevate") |

### 6.2 Context Management Strategies
- **Master Layer**: Conversation summarization (compress early history, keep last ~10 turns); hierarchical context (System > Session Summary > Recent Dialogue > Retrieved snippets)
- **Worker Layer**: Task decomposition; file scoping via `CLAUDE.md` (explicitly limit focus to relevant modules)
- **Retrieval**: Semantic search for `PAST_TASKS.md`; lazy loading (only fetch when explicitly needed)

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
4. **Gemini 3 Flash** transcribes (supports Chinese, English, mixed)
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
- Primary: **Gemini 3 Flash**
- Fallback: **Kimi k2.5** (automatic switch on API failure, transparent to user)

### 9.2 Quota Exhaustion Handling
- Worker enters **PENDING_QUOTA** state on quota error
- Queue Manager (Python) periodically polls for quota recovery
- Task auto-resumes when quota available; Master Agent notified to inform user
- All context persisted to disk during wait

### 9.3 Merge Conflict Resolution
When auto-merge fails:
1. Spawn **Conflict Resolution Worker**
2. Worker reads commit messages from both branches
3. Analyzes conflicted files and context
4. Decides: keep one side, merge both, or manual resolve
5. Creates resolution commit with explanation

### 9.4 Backup Strategy
- **Hourly automatic backups**: Critical data (MEMORY.md, PAST_TASKS.md, metadata) to `~/.charliebot/backups/`
- **Git-based**: Worktree contents backed up via frequent commits
- **Retention**: 7 days with daily snapshots beyond

---

## 10. Development Guidelines

### 10.1 Code Style
- **Standard**: Google Code Style (2-space indent, 120 column limit)
- **Python**: Enforced via YAPF (`.yapf`):
  ```ini
  [style]
  based_on_style = google
  indent_width = 2
  split_before_first_argument = true
  column_limit = 120
  ```

### 10.2 Worker Instructions (CLAUDE.md)
Each Thread's `CLAUDE.md` contains:
1. Default shared instructions from `config/claude-default.md` (coding standards, YOLO mode, git conventions)
2. Specific task description and objectives
3. Session-specific context/constraints
4. References to relevant files from PAST_TASKS.md

---

## 11. Implementation Status

### 11.1 Completed (MVP)

**Backend**
- FastAPI server (`server.py`) with APScheduler hourly backup task
- All API routes: `/api/sessions`, `/api/chat`, `/api/threads`, `/api/voice`, `/api/memory`
- `MasterAgent` with Gemini (primary) + Kimi (fallback) LLM providers, streaming, conversation summarization
- `QueueManager` — priority queue (P0 > P1 > P2) with atomic JSON persistence via `os.replace`
- `SessionDispatcher` — per-session background queue loop that pops tasks, creates threads, and spawns Claude Code workers (`claude -p --dangerously-skip-permissions --output-format stream-json --verbose`)
- Worker completion handling: dispatcher reads worker's NDJSON event log, asks Master Agent to review and summarize, appends summary to session conversation history, broadcasts via session WebSocket
- `SessionManager`, `ThreadManager`, `MemoryManager`, `BackupManager`, `GitManager`
- `init_charliebot_home()` — seeds `~/.charliebot/` on first run, populating API keys from env vars into `config.yaml`
- Memory updates: Master Agent can include `memory_update` field in any response to persist user preferences/facts to `MEMORY.md`

**WebSocket Endpoints**
- `/ws/sessions/{session_id}` — session-level events (worker completion summaries pushed to chat)
- `/ws/threads/{thread_id}` — thread-level events (live Worker NDJSON output streaming)

**Frontend**
- React SPA (Vite + TypeScript) built to `web/static/`, served by FastAPI StaticFiles (Node.js/npm is build-time only)
- Panels: Sessions sidebar, Chat (SSE streaming), Threads list, Plan review checklist, Voice push-to-talk
- ChatPanel subscribes to session WebSocket — receives worker summaries and renders them as assistant messages
- ThreadsPanel polls every 3 seconds for thread status updates
- No-cache middleware on HTML to prevent stale JS bundles

**Configuration**
- `~/.charliebot/config.yaml` is the single source of truth for API keys and settings
- Env vars (`GEMINI_API_KEY`, `KIMI_API_KEY`) used only to seed config on first run — never read at runtime
- Active models: `gemini-2.0-flash-thinking-exp-01-21` (primary), `kimi-k2.5` (fallback)

**Tests**
- 126 unit tests across: models, config, queue, streaming, memory, backup, sessions, master agent, server utilities
- 47 UAT tests via Starlette `TestClient` covering all API endpoints end-to-end
- 3 live Gemini integration tests (skipped if API key absent)

### 11.2 Known Issues / Fixed Bugs

| Bug | Fix |
|-----|-----|
| `backup.py` crashed on rapid successive backups | `mkdir()` → `mkdir(exist_ok=True)` |
| `GET /api/sessions` → 404, `POST /api/sessions` → 405 | StaticFiles mount at `/` intercepts before FastAPI `redirect_slashes`; fixed by adding trailing slashes in `sessions.ts` |
| 14 TypeScript build errors blocking frontend | Removed unused imports, fixed `thread.plan_steps` (not on `ThreadMetadata`), added explicit types |
| Master Agent refused git/shell task delegation | System prompt said "git operations" as off-limits; reworded to delegate ALL actionable tasks |
| Gemini API key not found at runtime | Config used `CHARLIEBOT_GEMINI_API_KEY` prefix; fixed by seeding key into `config.yaml` at init |
| Browser served stale JS bundle | Added no-cache middleware for HTML responses in `server.py` |
| `WorkerEventLog` TypeError on non-string `event.message` | Wrapped with `String()` before calling `.slice()` |
| Threads not visible in UI after delegation | Thread created inline in `chat.py` before SSE completes; ThreadsPanel polls every 3s |
| Master Agent silent after worker completion | Added `_notify_completion` in dispatcher → reviews results → pushes summary via session WebSocket |

### 11.3 Pending / Not Yet Implemented

- Git worktree isolation per thread (branch creation, checkout)
- Conflict Resolution Worker
- Semantic search over `PAST_TASKS.md`
- Claude plan usage tracking and Kimi K2.5 fallback for workers when near quota limits
