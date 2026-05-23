# CharlieBot

A Python-based orchestration system that coordinates multiple Claude Code worker instances to complete complex tasks via a responsive Web UI.

## Features

- **Multi-Agent Orchestration** — Master agent coordinates Worker and Reviewer agents across pluggable backends (Claude Code, Codex, Kimi, Gemini CLI, OpenCode, Antigravity CLI)
- **Task Delegation** — One-shot coding tasks dispatched to workers in isolated git worktrees with automatic cross-backend code review and ff-only merge
- **Iterative Improvement (/improve)** — Autonomous multi-iteration loop where each worker builds on prior results toward a specified goal
- **Scheduled Tasks (Cron)** — Configurable cron jobs in three modes: prompt (send to master), handler (run Python function), loop (backlog-driven improvement)
- **Delayed Triggers** — One-shot timed wake-ups for monitoring long-running background processes (training, builds)
- **Slash Commands** — Built-in + user-defined commands (shell or prompt scope), hot-reloaded from YAML config
- **Backlog Management** — YAML-based project backlog with status tracking (pending → approved → in_progress → done), priority levels, and auto-commit to git
- **Web UI** — React SPA with sessions sidebar, streaming chat, threads panel, and real-time WebSocket updates
- **Voice Input** — Push-to-talk recording with Gemini transcription supporting Chinese, English, and mixed input
- **Skills System** — Shared and host-specific skill knowledge files, auto-loaded by context relevance
- **Memory and Knowledge** — Persistent user preferences (MEMORY.md), host facts (MEMORY.host.md), and per-project skill docs
- **Session Operations** — Fork, elone (fork + re-run first message), archive, star, rate, search, and rewind
- **File Uploads** — Attach files to sessions for agent access
- **Backup** — Compressed archive backups with tiered retention policy
- **Code Server Integration** — Self-hosted VS Code Web (code-server) for browser-based code browsing with full IDE features
