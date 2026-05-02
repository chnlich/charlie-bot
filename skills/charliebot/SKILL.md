---
name: charliebot
description: CharlieBot repo structure, architecture, and development conventions. Use when modifying charlie-bot code.
version: 1.0.0
---

> For CharlieBot capabilities (delegation, /improve, triggers, etc.), see prompts/master.md — that content is auto-loaded into every master agent session.

# CharlieBot

You are CharlieBot. This skill describes your own features so you can use them correctly.
Source code: `~/workspace/charlie-bot/src/core/`

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
- `schedule_trigger --watch-pid` is local-host only (uses `pidfd_open`); do NOT pass a PID from a remote host / SSH job — it will fire `pid_gone` immediately. For remote tasks, use a log marker or sentinel file with a regular delay-based trigger.
- **Remote `nohup`/`setsid` background launches must be verified before yielding the turn.** Non-interactive SSH does not source login rc files, so PATH-dependent commands (e.g. tools installed under `~/.local/bin`) silently exit immediately with `command not found`. Required pattern:
  1. Use absolute paths, or wrap the command with `bash -lc "..."` so PATH is correct.
  2. After launch, `sleep 30` then run `ssh host 'tail -N <log>'` AND `ssh host 'kill -0 $REMOTE_PID'` to verify the process is actually alive and producing real output (not just shell setup noise).
  3. Only after both checks pass, schedule the long delay-based trigger and yield.

  Without this verification, a `nohup` exit-on-fail is invisible until the trigger fires hours later — wasted wall time.

---

## Sidebar & Frontend

- New sidebar filter panels: When adding a filter panel (like "scheduled"), also add the filter name to the URL filter restoration array in `web/static/js/app.js` (`switchSidebarFilter` init). Without it, the page load doesn't restore the filter and falls back to "All".

---

## Skills System

Shared skills live in `<charlie-bot-repo>/skills/`; host-specific skills live in
`~/.charliebot/skills/`. Both sync into `~/.claude/skills/`.
Skills with `user-invocable: false` are auto-loaded by CC when contextually relevant.

---

## Code Server (VS Code Web)

A self-hosted VS Code instance running as a web service for browsing code in the browser with full IDE features (syntax highlighting, file tree, search, go-to-definition).

**Setup:**
1. Install (one-time, no root needed):
   ```bash
   curl -fsSL https://code-server.dev/install.sh | sh -s -- --method standalone
   ```
   Binary installs to `~/.local/bin/code-server`.

2. Config at `~/.config/code-server/config.yaml`:
   ```yaml
   bind-addr: 0.0.0.0:<PORT>
   auth: none
   cert: <path-to-tls-cert>
   cert-key: <path-to-tls-key>
   ```
   Port and TLS cert paths are host-specific — see HOST MEMORY in MEMORY.md for the current host values.

3. Start:
   ```bash
   ~/.local/bin/code-server --disable-telemetry --disable-update-check <default-folder>
   ```

**Usage:**
- Open any folder via URL: `https://<host>:<port>/?folder=/path/to/dir`
- Can browse any filesystem path — not limited to the startup directory
- No authentication needed when behind Tailscale
- Shares the same TLS cert as CharlieBot

**Features:** File tree, syntax highlighting, Ctrl+P (quick open), Ctrl+Shift+F (project search), Ctrl+Click (go-to-definition with language extensions), minimap, git diff view.

**Note:** Not auto-started. Run the start command manually. Process is not managed by CharlieBot.

---

## Backup

Compressed archive backups (not git) stored at `~/.charliebot_backup`, tiered retention. Manual backup trigger must be independent and must not affect the auto-backup schedule.

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
| Delayed triggers | `triggers.py` |
| Sessions | `sessions.py` |
| Config | `config.py` |

Never guess how CharlieBot works — the source code is always available.
