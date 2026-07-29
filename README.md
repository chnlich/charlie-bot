# CharlieBot

A Python-based orchestration system that coordinates multiple Claude Code worker instances to complete complex tasks via a responsive Web UI.

## Setup

Run `./scripts/setup.sh` on a new host. It syncs skills, provisions `~/.charliebot/` (home layout, memory store scaffold, and `config.yaml` from `configs/config.example.yaml`), seeds repo-owned default cron tasks into `~/.charliebot/config.d/cron.yaml`, and prints the effective scheduled task list. The server startup path provisions the home layout too, but it never writes `cron.yaml` — only `setup.sh` does — so re-run `setup.sh` after pulling to pick up newly added repo-default cron tasks (same rule that already applies to new repo skills). Fill in secrets such as `gemini_api_key` and `charliebot_access_key` before the first start.

## Features

- **Multi-Agent Orchestration** — Master agent coordinates Worker and Reviewer agents across pluggable backends (Claude Code, Kimi, DeepSeek SGLang, Codex, Charlie Code, Gemini CLI, OpenCode, Antigravity CLI)
- **Task Delegation** — Implement, quick-edit, or script-run tasks in isolated git worktrees; implement tasks get review and ff-only merge
- **Iterative Improvement (/improve)** — Autonomous multi-iteration loop where each worker builds on prior results toward a specified goal
- **Scheduled Tasks (Cron)** — Configurable cron jobs in three modes: prompt (spawn worker), handler (run Python function), loop (backlog-driven improvement)
- **Delayed Triggers** — One-shot timed wake-ups or watch-based alerts for local PIDs, remote PIDs, and SLURM jobs
- **Slash Commands** — Built-in + user-defined commands (shell or prompt scope), hot-reloaded from YAML config
- **Backlog Management** — YAML-based project backlog with status tracking (pending → approved → in_progress → done), priority levels, and auto-commit to git
- **Web UI** — Vanilla JavaScript UI with sessions sidebar, streaming chat, threads panel, and real-time WebSocket updates
- **Voice Input** — Push-to-talk recording with Gemini transcription supporting Chinese, English, and mixed input
- **Skills System** — Shared and host-specific skill knowledge files, auto-loaded by context relevance
- **Memory and Knowledge** — Labeled-entry store at `~/.charliebot/memory/` (one fact per file, tagged by scope/topic/audience): resident topics inject in full at master spawn, topic-matched entries at worker spawn, queryable mid-session via `charliebot memory`; plus per-project skill docs
- **Session Operations** — Clone/fork, Elon-e takeover, archive, star, rate, and search
- **File Uploads** — Attach files to sessions for agent access
- **Backup** — Compressed archive backups with tiered retention policy
- **Code Server Integration** — Self-hosted VS Code Web (code-server) for browser-based code browsing with full IDE features
