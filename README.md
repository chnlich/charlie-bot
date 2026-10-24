# CharlieBot

CharlieBot is a self-hosted multi-agent orchestration system: a master agent drives worker and reviewer agents across pluggable LLM CLI backends, operated through a web UI.

## Features

### Orchestration

- Master/worker/reviewer delegation in isolated git worktrees, integrated by fast-forward-only pushes to the base branch
- Pluggable backends: Claude Code, Anthropic-compatible, OpenAI-compatible, Codex, Gemini CLI, OpenCode, Charlie Code, Antigravity CLI
- Chat file uploads: text attachments ride the message's path text; image attachments reach the Charlie Code CLI as repeated `--image` flags by default, and are refused with one error event (nothing sent) when the backend entry sets `image_input: false`
- `charliebot improve` autonomous change-run-verify iteration loops
- Plan registry (`charliebot plan`) with HTML plan artifacts and anchored comments

### Automation

- Cron scheduled tasks in prompt, handler, loop, and task-chain (`steps`) modes
- Manual run of a scheduled task: `POST /api/cron/tasks/{name}/run` fires it once through the scheduled path
- Delayed triggers (`charliebot schedule-trigger`) watching local/remote PIDs and SLURM jobs; one trigger can watch many targets
- `charliebot schedule-trigger --cmd` starts a long-running command detached (locally or over ssh) and registers its wake; the wake carries the exit code and log path

### Knowledge

- Memory store: labeled entries with topic/audience scoping, queried mid-session via `charliebot memory`
- Skills system: repo-shared (`skills/`) plus host-specific skills

### Web UI

- Streaming chat over WebSockets
- Sessions sidebar with groups, filters, and search; star / archive / fork sessions
- Worker (thread) and plan panels
- HTML artifact viewer with line-anchored comments
- GitHub-style diff viewer with diff comments
- Perfetto and NCU trace viewers
- Built-in web terminal (tmux)
- Voice input with online speech transcription (Gemini and Muse backends)
- Usage panels: external provider quotas and per-session context usage
- File browser (`/absolute_filepath/`) and file uploads
- Access-key auth

### Integrations

- code-server proxying (browser VS Code)
- Anthropic Messages-compatible proxy endpoint (`POST /api/anthropic-proxy/openai-compatible/{backend_id}/v1/messages`) fronting OpenAI-compatible backends
- Slack and Discord summon entrypoints on a shared thread core — see [docs/discord.md](docs/discord.md) and [docs/chat-platforms.md](docs/chat-platforms.md)

## Screenshots

<p align="center">
  <img src="docs/assets/readme/ui-chat-desktop.png" alt="Desktop chat view" width="880">
</p>
<p align="center"><i>Desktop: chat view with the external-provider quota strip (Claude/Codex) and the per-session context usage indicator. Dollar values shown are fabricated demo data.</i></p>
<p align="center">
  <img src="docs/assets/readme/ui-chat-mobile.png" alt="Mobile chat view" width="320">
</p>
<p align="center"><i>Mobile: the same chat and session list in a phone-width layout.</i></p>
<p align="center">
  <img src="docs/assets/readme/ui-plans-desktop.png" alt="Plans tab" width="880">
</p>
<p align="center"><i>Plans: an approved plan's HTML artifact in the viewer — plan/version switchers, revision badges, and resolved trade-offs from the approval lifecycle.</i></p>

## Prerequisites

- Python 3.14 (uv installs it) and git (worker tasks run in git worktrees)
- Node.js is only needed for frontend development

## Quick start

```bash
./scripts/setup.sh   # sync skills, provision $CHARLIEBOT_HOME (default ~/.charliebot), seed repo-default cron tasks
# fill in the secret in $CHARLIEBOT_HOME/credentials.yaml: charliebot.access_key
./scripts/start-server.sh
```

`scripts/switch-server.sh rust <full rust-backend sha>` moves the live server onto a verified Rust release and `scripts/switch-server.sh python` moves it back. The switch ends with a probe turn in a session of its own: it passes once the new server records the run's first assistant output (or the run ends success), then archives the session after stopping every run the session still has open, including a follow-up run the server starts in it. The script header lists the preflight and the exit codes.

`setup.sh` provisions `$CHARLIEBOT_HOME/` (home layout, `config.yaml` from `configs/config.example.yaml`, `credentials.yaml` from `configs/credentials.example.yaml`) and seeds repo-default cron tasks into `$CHARLIEBOT_HOME/config.d/cron.d/<name>.yaml` (one file per job). Server startup never writes cron config — only `setup.sh` does — so re-run `setup.sh` after pulling to pick up new repo-default cron tasks and skills.

## CLI at a glance

- `charliebot artifact` — check a page's genre DOM assertions and cold-read probe, or wrap a content fragment into a genre page
- `charliebot compaction-context` — restore the skills a manager session read before Claude compacted its context
- `charliebot config` — print a config key's value through the loader (`config get <key>`)
- `charliebot delegate` — delegate a task to a worker agent
- `charliebot discord` — reply to or read the Discord thread a session was summoned from, and check the bot setup
- `charliebot disk-sweep` — list or remove old task worktrees and scratch directories (`--yes` applies; without it the run is a read-only dry run)
- `charliebot improve` — start an iterative improvement loop
- `charliebot improve-stop` — stop this session's running improvement loop after the current iteration
- `charliebot memory` — query and lint the store; `add` stages a capture
- `charliebot plan` — present (register), amend, approve, close, diff, and list a session's plans
- `charliebot publish` — publish an artifact to the URL readers beyond the operator's devices open
- `charliebot router` — choose backends for one task by asking a decision model whether the work is routine or design work
- `charliebot schedule-trigger` — schedule a delayed trigger (timed, watching PIDs/SLURM jobs, or starting a command with `--cmd`)
- `charliebot session` — create a session; `send` relays a message to another session as an agent_message
- `charliebot slack` — reply to or ack the Slack thread a session was summoned from
- `charliebot storage` — `cool` deletes the bytes no reader can reach again: cold sessions' transport files and cold-or-orphaned backend records
- `charliebot voice-replay-eval` — replay recorded dictations through the transcription backends and score latency and accuracy
- `charliebot usage-ledger` — `capture` writes this host's per-call usage into the ledger
- `charliebot gc-trash` — inspect and purge quarantined worktree trash
- `charliebot web` — reply to a web session, print its For-you layer as plain text, or set and show its status

## Repository layout

- `src/infra/` — config, models, logging, process, git and HTTP helpers; imports nothing else from `src`
- `src/runtime/` — sessions, task tree, master and worker runs, agent processes, core API routers and CLI commands
- `src/backends/` — one package per agent backend (`claude_code`, `codex`, `opencode`, ...)
- `src/features/` — one package per feature (`cron`, `improve`, `slack`, `usage`, ...) with its logic, `api.py` and `cli.py`
- `src/app/` — composition root: page routes and the `charliebot` command table
- `web/` — Jinja2 templates + vanilla JS static assets
- `skills/` — repo-shared agent skills
- `prompts/` — agent prompt definitions
- `scripts/` — operator scripts: setup, server launcher, skill sync, charlie-code preflight
- `tools/` — maintenance tools: style and leak checks, git hooks, browser and live harnesses, CSS build
- `tests/` — pytest suite; `tests/test_package_structure.py` fails on an import that crosses the `src` packages against their directions
- `configs/` — example config and repo-default cron tasks
- `docs/` — supplementary documentation and assets

## License

MIT — see [LICENSE](LICENSE).
