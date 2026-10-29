# CharlieBot Project Specification

## 1. Project Overview & Objectives
**CharlieBot** is a system designed to coordinate and manage multiple **Claude Code** instances (Workers) to complete complex tasks. The primary interface is a responsive Web UI for desktop and mobile access. The server and the `charliebot` CLI are the Rust release under `rust/crates/`.

### 1.1 Objectives
- **Agent Orchestration**: A **Master Agent** (running as a Claude Code session) manages and coordinates Worker instances via a CLI delegation workflow.
- **Task Delegation**: Master delegates coding tasks to Workers that run in isolated git worktrees; a separate Review Agent automatically verifies and merges the work.
- **Rust server and CLI**: both run from `rust/crates/`, the server as `charliebot-server` and the CLI as the `charliebot` binary built beside it.

---

## 2. Technical Stack
- **Language**: Rust (`rust/crates/`, axum and tokio) for the server and the CLI
- **Master Agent**: Claude Code session (pluggable backends)
- **Worker Agent**: Claude Code (local CLI invocation, non-interactive mode)
- **Backend**: the Rust server (`charliebot-server`, entry point `rust/crates/app/server/src/main.rs`), WebSockets for real-time streaming, tokio for concurrency
- **Frontend**: vanilla JS + Tailwind CSS, served by the Rust server's static mounts (`rust/crates/infra/compat/src/static_files.rs`)
- **Storage**: JSON for state/data, YAML for configuration

---

## 3. Directory Structure

### 3.1 Home Directory (`~/.charliebot/` or `CHARLIEBOT_HOME`)
All instance-specific data (configs, sessions, memory) is stored here.

`CHARLIEBOT_HOME` selects which one: unset gives `~/.charliebot`, and a set value (absolute
or `~`-prefixed; relative is rejected) gives a separate profile, seeded on first use. Several
profiles run side by side on one host, each with its own port in its own `config.yaml`. The
home path is resolved once per side, `charliebot_home_dir()` in `rust/crates/infra/core/src/home.rs`;
every other path derives from `CharlieBotConfig.charliebot_home`. One raw read of the variable
sits outside them: the web terminal's profile check (`rust/crates/features/terminal/src/terminal.rs`). A tmux
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
        └── data/              # Session-level JSON data
            └── runs/              # Run directories
                └── {run_id}/
                    ├── metadata.json    # Run record (identity, backend, status, timestamps)
                    ├── task_spec.md     # The run's pinned task-spec body
                    ├── events.jsonl     # The run's translated transport events
                    ├── agent.raw.ndjson # The CLI's raw stream
                    └── agent.stderr.log # The CLI's stderr
```

**Worktrees** are stored under `worktree_dir` (config, default `~/worktrees`), one directory per
thread branch — the directory name is the branch name with `/` replaced by `-`:

```text
<worktree_dir>/
└── charliebot-task-{ts}-{id}/      # Thread worktree (isolated branch `charliebot/task-{ts}-{id}`)
```

**Notes:**
- Individual Worker logs are in `data/runs/{run_id}/`: the raw-log spawn writes `agent.raw.ndjson` (the
  CLI's stream) plus `agent.stderr.log`; the pipe transports write `stdout.log` plus `stderr.log`;
  `events.jsonl` holds the translated events.
- `workspace_dirs`: Config option (`config.yaml`) listing workspace directories to scan for git projects. The diff page's repo picker lists the discovered projects.

### 3.2 Repository Code Structure (Stateless)
```text
charlie-bot/
├── rust/crates/        # The Rust server and CLI crates; the server's entry point is app/server
├── src/                # The server's page templates, static assets and helper scripts, read by path
├── web/                # Web UI (templates/ + static/)
├── configs/            # Default templates and examples
└── project.md          # This specification
```

---

## 4. Core Architecture

### 4.1 Agent Roles
| Role | Type | Responsibilities |
|------|------|------------------|
| **Master Agent** | Claude Code session (`rust/crates/runtime/tasks/src/master_run.rs`) | User interaction, high-level planning, delegating coding tasks to Workers, reviewing combined worker+reviewer results. Runs as a persistent Claude Code subprocess with `--resume` support across messages. Can use any configured backend. |
| **Worker Agent** | Claude Code CLI (`rust/crates/runtime/tasks/src/worker.rs`) | Code analysis, implementation, file editing, git operations, testing. Runs in an isolated git worktree on a dedicated branch. Told NOT to rebase/merge/remove the worktree — a reviewer handles that. |
| **Review Agent** | Claude Code CLI (same run loop as the Worker, `rust/crates/runtime/tasks/src/review.rs`) | Automatically spawned after a Worker succeeds. Reviews the diff, fixes issues, rebases onto the remote base, and pushes the branch to the base (git rejects a non-fast-forward push). Intentionally uses a DIFFERENT backend than the Worker (cross-backend review via `backends.pool`). |

**Backend Abstraction**: Workers and Master use a pluggable `AgentBackend` trait (`rust/crates/runtime/registry/src/registry.rs`). Each backend package's `register()` hands the registry a `BackendTypeRegistration` (`registry.backend_type` in `rust/crates/runtime/registry/src/registry.rs`) that carries its type string, option model and factory, and the registry builds each `BackendOption.type` through that factory; config loading parses each `backends.options[]` entry through the option models each backend's `options.rs` holds. Backend selection is configured via `backends.options` and `backends.pool` in `config.yaml`.

**Turn Contributions**: Feature packages add to a master turn through the registry's `TurnContribution` trait (`rust/crates/runtime/registry/src/registry.rs`): a package registers a `TurnContribution` from its `register()`, and the turn path (instruction build, queue, run, session funnel, chat aggregator) asks every registered contribution at its own step and names no feature. The chat-threads, cron, Discord, memory, Slack and voice packages register one each.

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
   - Master calls `charliebot delegate --repo /path --base-branch main --task-spec-file <file>` (`rust/crates/app/cli/src/commands/delegate.rs`); session identity comes from the `CHARLIEBOT_SESSION_ID` the server writes into the master process, with cwd as the fallback when it is absent
   - The CLI POSTs to `/api/internal/delegate`, which creates the worker task-tree child and registers its first work Run (`delegate_task_tree` in `rust/crates/runtime/tasks/src/routes.rs`); the tree's executor launches it (`execute_run` in `rust/crates/runtime/tasks/src/execution.rs`)

2. **Worker Execution** (Phase 1 — Implement):
   - Spawner creates an isolated git worktree on a new branch (`charliebot/task-{ts}-{id}`)
   - Worker receives a prompt with session info, worktree instructions, and the task description
   - Worker is explicitly told: "Do NOT rebase, merge, or remove the worktree. A reviewer will handle that."
   - Worker implements the task, commits changes, and exits
   - Events are streamed via WebSocket and persisted to `events.jsonl`

3. **Review** (Phase 2 — Automatic on Worker Success):
   - On successful work-run completion, `maybe_spawn_review` (`rust/crates/runtime/tasks/src/execution.rs`) registers a review Run on the same task and launches it, picking the reviewer backend with `select_reviewer_backend` (`rust/crates/runtime/tasks/src/review.rs`)
   - The reviewer intentionally uses a **different LLM backend** than the worker (cross-backend review), selected from the `backends.pool` rotation
   - Reviewer reads session conversation + worker log for context, then:
     - Reviews `git diff base_branch...branch_name`
     - Fixes any issues found, commits fixes
     - Rebases the branch onto the base branch
     - Pushes the rebased branch to the remote base (`git push origin HEAD:{base_branch}`); git rejects a non-fast-forward push
     - Cleans up the worktree
   - If the reviewer **fails**, it retries with the next untried member of `backends.pool` (cooling members and the members the worker used are skipped)

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

- **Draft & present**: The master drafts the plan as an HTML artifact (`artifacts/plan_NN.html`, grammar in `prompts/plan_template.html`) and registers it with `charliebot plan present --file <artifact> --title <title>` (the CLI command `rust/crates/features/artifacts/src/cli/plan.rs` over the server's `present` verb in `rust/crates/features/artifacts/src/plan_verbs.rs`). `charliebot plan amend --file <artifact> --note <why>` appends the next version (trigger: `auto_amend` or `feedback`).
- **Review**: The plan renders in the web Plans panel with a version switcher, a diff toggle against the predecessor, and block-anchored comments (`src/features/artifacts/static/plan-panel.js`).
- **Approve**: After the plan's Improve round and verify, the parent node or the root line approves it; `charliebot plan approve` records the approval against the latest version. The runtime gate (`rust/crates/runtime/store/src/task_tree.rs`, `is_read_only_exempt` and the delegation refusals) admits `/delegate` and `/improve` when the calling manager and its ancestors are open and the chain holds a real user message or a thread platform summon.
- **Close**: `charliebot plan close --plan N --as superseded|abandoned|completed` terminates the lineage.

---

## 6. Memory & Knowledge Management

### 6.1 Labeled-Entry Memory Store
A local git repo at `$CHARLIEBOT_HOME/memory/` holds one durable fact or rule set per file under
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
4. **Online transcription** sends the complete recording through the selected backend (`gemini` or `muse`; `voice.default_backend` when the request names none). No speech model runs on the server.
5. Transcription displayed in UI first
6. Passed to Master with a note: the displayed message stays verbatim, the web client sends the message with `input_mode: "voice"`, and the voice package's turn contribution (`rust/crates/features/voice/src/lib.rs`) opens the prompt's copy of the message with the fixed voice note (`VOICE_NOTE`)

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
`AgentBackend.translate_event` (the `translate` method on `AgentBackend` in `rust/crates/runtime/registry/src/registry.rs`, driven by the runner in `rust/crates/runtime/agents/src/runner.rs`) turns each line into the events that
land in `events.jsonl` and the WebSocket stream: the Anthropic-endpoint backends (cc-claude,
cc-anthropic-compatible, cc-openai-compatible) pass lines through unchanged; the other backends translate their
native streams into CC-compatible events with per-backend vocabularies (codex, for one, emits
`thinking` and `file_write`).
A raw log that stops growing while the process is still alive is what the server reports as stuck
(the no-output silence report); thinking in progress is the `thinking` content.

---

## 9. Error Handling & Resilience

### 9.1 Model Fallback
- Multiple backends configured via `backends.options` in `config.yaml` (see that file for the current list)
- `backends.pool` is the rotation of unpinned work (the four worker task types, the improve loop, the reviewer, the cold read, the session namer and the page check); a member whose quota runs out cools until its reset, or 30 minutes when none is named, and a worker interrupted by quota continues on the next member (`rust/crates/runtime/agents/src/backend_pool.rs`)
- Failed reviewers automatically retry with the next untried backend

### 9.2 Rebase Conflict Handling
- The Review Agent rebases the worker branch onto the base branch before merging
- If the rebase fails (conflicts), the reviewer attempts to resolve them as part of its review
- If unresolvable, the review fails and retries with the next backend

---

## 10. Development Guidelines

### 10.1 Code Style
- **Standard**: Google Code Style (2-space indent, 120 column limit), applied to the Rust code by rustfmt
  (`rust/rustfmt.toml`)
- **Rust**: CI (`.github/workflows/ci.yml`) runs `cargo clippy --all-targets --locked -- -D warnings` and
  `cargo fmt --check` from `rust/` on every pull request and push

### 10.2 Worker Instructions
Each worker and reviewer launch writes its managed instructions to `harness_instructions.md` in its Run directory
(`HARNESS_INSTRUCTIONS_NAME`, `rust/crates/runtime/tasks/src/worker.rs`). The text is the run's `PromptSnapshot.instructions_text`
(`rust/crates/runtime/tasks/src/prompts/snapshot.rs`): the worker role rules from `prompts/worker.md`, `review_rules_text`
(`rust/crates/runtime/tasks/src/review_text.rs`) for reviewers, and the memory block. The worktree is never written, so the checked-out
repo's own guide files stay in effect for the agent. Each backend passes the instructions on every launch,
including a resumed run and a pool relay:

- Claude Code: `--append-system-prompt-file <run dir>/harness_instructions.md`.
- charlie-code: `--instructions-file <run dir>/harness_instructions.md`.
- Codex: `-c developer_instructions=<file text as a TOML string>` and `-c project_doc_fallback_filenames=["CLAUDE.md"]`.
- OpenCode: the base class frames the text into the prompt.

A run on a repository starts in its worktree root, so its CLI can read that repository's guide and skills from there;
a run without a repository starts in its Run directory. A manager turn is the one path that writes an instructions
file into a working directory: the backend writes it into the session cwd (CLAUDE.md for Claude Code, AGENTS.md
for the other backends) and passes no flag.

---

## 11. Implementation Status

### 11.1 Completed (MVP)

**Backend**
- Rust server (`rust/crates/app/server/src/main.rs`)
- All API routes: `/api/sessions`, `/api/chat`, `/api/internal/delegate` (full list: the route tables in `rust/crates/app/server/src/routes` and the packages in `rust/crates/app/server/src/packages/mod.rs`)
- Master Agent as Claude Code session (`rust/crates/runtime/tasks/src/master_run.rs`) with `--resume` support for persistent conversations. Supports any configured backend via the pluggable `AgentBackend` trait (`rust/crates/runtime/registry/src/registry.rs`)
- Delegation CLI (`rust/crates/app/cli/src/commands/delegate.rs`) — called by the master to spawn workers via `POST /api/internal/delegate`
- Worker spawner (`rust/crates/runtime/tasks/src/execution.rs`, `rust/crates/runtime/tasks/src/spawner_prompt.rs`, `rust/crates/runtime/tasks/src/spawner_backends.rs`) — creates isolated git worktrees, builds enriched prompts, spawns workers, and orchestrates the two-phase worker+reviewer pipeline
- Automatic cross-backend review: on worker success, a Review Agent is spawned using a different LLM backend (configurable via `backends.pool`). Failed reviewers retry with the next untried backend
- Master trigger on completion: combined worker+reviewer summary is sent to the master agent via `trigger_master()` for user notification and follow-up decisions
- Session blocks (`rust/crates/runtime/store/src/session_events.rs` and the other `session_*.rs` blocks), `PlanRegistryManager` (`rust/crates/features/artifacts/src/plans.rs`), `TriggerManager` (`rust/crates/runtime/triggers/src/triggers.rs`), `StreamingManager` (`rust/crates/runtime/store/src/streaming.rs`)
- `init_charliebot_home()` (`rust/crates/app/server/src/home_init.rs`) — seeds `$CHARLIEBOT_HOME/` on first run with default `config.yaml`
- Memory store: `ensure_store` (`rust/crates/features/memory/src/memory.rs`) creates the scaffold (git repo + topics vocabulary) at first use, so a fresh home has no `memory/` directory until then
- Memory updates: sessions stage candidates via `charliebot memory add` (writes `staging/`, never `entries/`); the daily memory curator builds a user-approved diff that admits, revises, or evicts entries

**WebSocket Endpoints**
- `/ws/sessions/{session_id}` — session-level events (worker completion summaries pushed to chat)
- `/ws/terminal` — the profile's tmux-backed web terminal

Voice input records locally and ends one of two ways. With a live transcription backend the
browser also streams every audio chunk to the preview relay `/ws/voice/{session_id}`; on stop
the relay archives the recording it already received (under `sessions/{id}/voice/`, 16 kHz mono
PCM16 WAV plus a `.txt` with the final text) and pushes the final, and the browser uploads
nothing. The fallback — no final inside the budget or a relay failure — uploads the whole
recording: `POST /api/voice/{session_id}` persists it under `sessions/{id}/voice/`, then transcribes
it through the selected online backend and returns the text. A failed transcription keeps the
recording for the browser's retry.

**Frontend**
- Vanilla-JS UI under `web/static/js/`, served by the Rust server's static mounts (`rust/crates/infra/compat/src/static_files.rs`; Node.js/npm is build-time only: Tailwind CSS)
- Panels: the sessions sidebar plus the tab strip — Terminal, Plans, Chat
- The session WebSocket (`web/static/js/websocket.js`) drives the chat: rendering is fully driven by aggregated `message`/`stream` deltas, so a worker completion summary arrives as an assistant message
- Polls: the sidebar status poll adapts (3 s with running tasks, 10 s idle); an open session view polls its usage every 3 s while the master is thinking, and an open worker transcript polls every 2 s (all ride the page-timers registry, so a hidden tab polls nothing)
- Draft persistence: unsent message text is saved to localStorage per session (debounced 300ms) and restored on session switch-back or page reload

**Configuration**
- `$CHARLIEBOT_HOME/config.yaml` holds structure in sections (`server`, `paths`, `backends`, `accounts`, `voice`, `code_server`, `ui`, `slack`, `publish`, `telegram`); `$CHARLIEBOT_HOME/credentials.yaml` holds every secret as section → key and is the single source of truth for API keys — no environment variables
- `backends.options`: configurable list of LLM backends (see that file for the current list); an option id names the model family, never a version, and starts with a prefix its type's usage source registers (the id rule: usage's startup check, `rust/crates/features/usage/src/backend_ids.rs`, refuses an id outside those prefixes)
- `backends.pool`: ordered list of backend IDs in rotation order (`rust/crates/runtime/agents/src/backend_pool.rs`); server startup refuses a pool entry (the pool field validation in that file) or a cron task or step backend (cron's startup check, `backend_refs_check` in `rust/crates/features/cron/src/loader.rs`) that names no option id
