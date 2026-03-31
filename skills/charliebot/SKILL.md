---
name: charliebot
description: CharlieBot architecture, conventions, and feature reference. Use when modifying or discussing charlie-bot, its frontend, workers, sessions, scheduler, or backup system.
version: 1.0.0
---

# CharlieBot

You are CharlieBot. This skill describes your own features so you can use them correctly.
Source code: `~/workspace/charlie-bot/src/core/`

---

## /improve — Iterative Improvement Loop

**Source:** `src/core/improve_command.py`

**Usage:** `/improve [max_iterations] <goal>` — default 5 iterations.
**Stop:** `/stop-improve`

**How it works:**
1. System spawns a worker with the goal prompt
2. Worker makes changes in an isolated worktree → auto-review → merge
3. System spawns next worker with the goal + summaries of all previous iterations
4. Repeats until max_iterations or `/stop-improve`
5. Master agent receives final summary of all iterations

**Key mental model:**
- Workers are **fully autonomous** — they read code, make changes, run tests/training, verify results, all by themselves
- The user is **on the loop** (reviewing), not **in the loop** (executing)
- The goal prompt should specify **what to achieve** and **constraints**, NOT specific methods or directions — let workers explore creatively
- Workers inherit the session's repo path and subagent backend

**Master agent's role when user asks for /improve:**
1. Help the user formulate a concise goal string
2. Execute `/improve N goal` — that's it
3. Do NOT manually orchestrate delegation — the improve loop handles everything

---

## Delegation — One-Shot Task Spawning

**Source:** `src/core/spawner.py`

**Usage:**
```bash
python -m src.cli.delegate \
  --session SESSION_UUID \
  --repo /path/to/repo \
  --description "task description" \
  --context "optional context"
```

**How it works:**
1. Creates a worker thread in an isolated git worktree
2. Worker runs with the description as prompt
3. On success → auto-review agent checks and merges
4. Master receives combined worker+reviewer summary

**When to use delegation vs /improve:**
- **Delegation**: one-shot tasks with clear specifications (implement X, fix Y)
- **`/improve`**: iterative optimization toward a goal (improve performance, reduce errors)

---

## Slash Commands

**Source:** `src/core/slash_commands.py`
**Config:** `~/.charliebot/slash_commands.yaml` (hot-reloaded, no restart needed)

**Built-in commands:**
- `/help` — list all commands
- `/improve [N] <goal>` — iterative improvement loop
- `/stop-improve` — stop active improve loop
- `/run <task-name>` — manually trigger a scheduled task

**Custom commands** (defined in YAML):
- `scope: shell` — run a shell command, return stdout
- `scope: prompt` — inject a prompt into master agent

---

## Backlog & Improvement Loop (Scheduled)

**Source:** `src/core/improvement_loop.py`
**Config:** `~/.charliebot/config.d/cron.yaml`

A deterministic state machine for ongoing projects:
1. **Revision** — address reviewer feedback on existing items
2. **Implement** — pick highest-priority approved item
3. **Generate** — create new ideas if under cap
4. **Scan** — health scan if nothing else to do

Backlog items live in `loop/backlog.yaml` in the target repo.
Status flow: `pending` → `approved` → `in_progress` → `done`/`failed`

---

## Scheduled Tasks (Cron)

**Source:** `src/core/scheduler.py`
**Config:** `~/.charliebot/config.d/cron.yaml`

Task types: `prompt` (one-shot), `handler` (built-in), `loop` (backlog-driven).
Manage via `/run <name>` or the API.

---

## Worker & Review Workflow

**Source:** `src/core/spawner.py` (800+ lines)

1. Worker runs in isolated git worktree (`~/worktrees/`)
2. Branch naming: `charliebot/task-{timestamp}-{id}`
3. On worker success → reviewer auto-spawned (may use different backend via `model_preference`)
4. Reviewer: checks diff, fixes issues, rebases, merges `--ff-only`
5. On merge → master agent gets summary

---

## Repo-Specific Merge Policy

- Use git worktrees for branch operations by default.
- For `charlie-bot`, after a verified worktree change, it is okay to merge back into the main checkout automatically.
- For other repos, especially `your_project`, keep the main checkout untouched unless the user explicitly approves otherwise.

---

## Workers & Sessions — Architecture Notes

- **NEVER use `discover_repos()[0]` to get repo context for a derived/downstream task** (review workers, retries, continuations, chained tasks). `discover_repos()` returns repos in non-deterministic order. Always propagate `repo_path` explicitly from the originating task via `ThreadMetadata.repo_path`. `discover_repos()` is only safe at the top-level entry point (user delegation, CLI). This is a recurring bug — always propagate repo_path explicitly.
- Session CLAUDE.md: Each session gets a real `CLAUDE.md` file at `~/.charliebot/sessions/{id}/CLAUDE.md`, created by concatenating `MASTER_AGENT_PROMPT.md` + `MEMORY.md`. Done in `_ensure_master_claude_md()` (master_cc.py), called on every `run_message()`. Stale symlinks auto-removed.
- Worker log display: In main chat panel, only show "worker {id} started/ended" with general purpose description. Full logs belong in the worker panel only.
- Draft preservation: User's unsubmitted message text is preserved per-session when switching sessions.

---

## Sidebar & Frontend

- New sidebar filter panels: When adding a filter panel (like "scheduled"), also add the filter name to the URL filter restoration array in `web/static/js/app.js` (`switchSidebarFilter` init). Without it, the page load doesn't restore the filter and falls back to "All".

---

## Skills System

Custom CC skills live in `~/.charliebot/skills/`, symlinked to `~/.claude/skills/`. Skills with `user-invocable: false` are auto-loaded by CC when contextually relevant.

---

## Backup

Compressed archive backups (not git) stored at `~/.charliebot_backup`, tiered retention. Manual backup trigger must be independent and must not affect the auto-backup schedule.

---

## Voice Input

For voice-transcribed messages fed to CC, prepend: "The following message is from voice transcription and might not be accurate. Please ask first for any words that are unclear or might be wrong." Voice output (Gemini transcription) must be simplified Chinese or English only — never traditional Chinese.

---

## Improvement Decisions (Feb 2026)

**Not needed (single-user):** test suite, SQLite (JSON preferred), worker retry/backoff, worker resource limits, rate limiting/auth.
**Done:** session full-text search, error handling consistency, session rewind.
**Planned:** worker templates as slash commands.
**Deferred:** metrics/observability, notifications, multi-repo dashboard, semantic search, cost tracking, mobile UX, keyboard shortcuts.

---

## General Principle

**If you don't understand how a feature works, read the source code** at `~/workspace/charlie-bot/src/core/`. Key files:

| Feature | Source file |
|---------|------------|
| /improve loop | `improve_command.py` |
| Spawner + review | `spawner.py` |
| Slash commands | `slash_commands.py` |
| Backlog state machine | `improvement_loop.py` |
| Scheduler | `scheduler.py` |
| Sessions | `sessions.py` |
| Config | `config.py` |

Never guess how CharlieBot works — the source code is always available.
