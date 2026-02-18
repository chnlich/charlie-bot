# CharlieBot

A Python-based orchestration system that coordinates multiple **Claude Code** worker instances to complete complex tasks via a responsive Web UI.

## Overview

CharlieBot uses an API-based **Master Agent** (Gemini 3 Flash / Kimi k2.5) to manage and coordinate **Claude Code** Workers. The Master handles high-level orchestration while Workers perform actual code analysis, implementation, and testing in isolated Git worktrees.

### Key Features

- **Multi-Agent Orchestration**: One Master Agent coordinates multiple Claude Code Workers
- **Ralph Loop**: Continuous task consumption with priority queue (P0/P1/P2)
- **Git Isolation**: Each Worker operates on its own branch within session worktrees
- **Plan Mode**: Two-phase execution (planning → review → execution) for complex tasks
- **Voice Input**: Push-to-Talk with multilingual transcription (Chinese/English/mixed)
- **Real-Time Monitoring**: WebSocket/SSE streaming of Worker terminal output
- **Resilient**: Automatic model fallback, quota recovery, and intelligent conflict resolution

## Architecture

```
┌─────────────┐     ┌──────────────┐     ┌─────────────────┐
│   Web UI    │◄────┤ Master Agent │────►│  Worker Agents  │
│  (React)    │     │(Gemini/Kimi) │     │ (Claude Code)   │
└─────────────┘     └──────────────┘     └─────────────────┘
                            │                      │
                            ▼                      ▼
                   ┌─────────────────┐    ┌─────────────────┐
                   │  ~/.charliebot/ │    │  Git Worktrees  │
                   │  (State/Data)   │    │  (Isolated)     │
                   └─────────────────┘    └─────────────────┘
```

### Directory Structure

**Home Directory** (`~/.charliebot/`):
```
├── config.yaml       # API keys and settings
├── MEMORY.md         # Global user preferences/facts
├── PAST_TASKS.md     # Task history archive
├── PROGRESS.md       # Lessons learned
├── backups/          # Hourly snapshots
├── repos/            # Bare git clones
├── logs/             # Application logs
└── sessions/         # Session worktrees
    └── {uuid}/
        ├── task_queue.json
        ├── worktree/
        └── threads/
```

**Repository** (stateless application code):
```
├── src/              # Python backend (FastAPI)
├── web/              # React frontend
├── config/           # Default templates
└── server.py         # Entry point
```

## Quick Start

### Prerequisites

- Python 3.10+
- Node.js (for frontend development only)
- Git
- Claude Code CLI installed locally

### Installation

```bash
# Clone the repository
git clone https://github.com/chnlich/charlie-bot.git
cd charlie-bot

# Install Python dependencies
pip install -r requirements.txt

# Configure
cp config/config.example.yaml ~/.charliebot/config.yaml
# Edit ~/.charliebot/config.yaml with your API keys

# Run
python server.py
```

### Development

**Code Style**: Google Code Style (enforced via YAPF)
```ini
[style]
based_on_style = google
indent_width = 2
column_limit = 120
```

**Frontend Development**:
```bash
cd web/
npm install
npm run dev    # Development server
npm run build  # Production build
```

## How It Works

### 1. Ralph Loop (Task Queue)

CharlieBot operates in a continuous loop:
1. Master monitors priority queue (P0 Immediate, P1 Standard, P2 Background)
2. Spawns Workers on available slots
3. Workers execute and auto-exit
4. Master immediately triggers next iteration

### 2. Session & Thread Model

- **Session**: A project/workspace with its own Git worktree
- **Thread**: A single Worker task on an isolated Git branch
- Workers read `CLAUDE.md` for task instructions before execution

### 3. Plan Mode

For complex tasks:
1. Worker generates detailed plan (no file changes)
2. User reviews/edits plan in Web UI
3. Approved plan enters queue for execution
4. Multiple plans can run in parallel

## Configuration

See `config/config.example.yaml` for available options including:
- LLM API keys (Gemini, Kimi)
- Model preferences and fallback settings
- Session limits and queue configuration
- Voice transcription settings

## License

MIT
