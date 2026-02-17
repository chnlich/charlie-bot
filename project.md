# Project Spec: CharlieBot

## Project Overview
**CharlieBot** is a Python-based system designed to coordinate and manage multiple **Claude Code** instances to complete complex tasks. The primary interface for interaction is a responsive Web UI for desktop and mobile access.

## Objectives
- **Agent Orchestration**: Enable a "Master" agent to spawn, monitor, and manage "Worker" Claude Code instances.
- **Task Delegation**: Break down complex user requests into sub-tasks assigned to specialized Claude Code instances.
- **Python-Native**: Built entirely in Python for extensibility and ease of integration with AI toolsets.

## Key Design Principles

### 1. Separation of Code and Configuration
- **Stateless Application**: The core logic remains in the installation/repository directory and is intended to be shared across multiple instances.
- **Per-Instance Configuration**: All instance-specific data (database, configs, logs, session state) is stored in a dedicated user-directory, following the pattern of tools like OpenClaw.
- **Home Directory Path**: Default path will be `~/.charliebot/` (or configurable via environment variable `CHARLIEBOT_HOME`).
- **Structure of Home Directory**:
  ```text
  ~/.charliebot/
  ├── config.yaml      # Instance-specific configuration (API keys)
  ├── data/            # Persistence (SQLite database)
  ├── repos/           # Shared storage for base git repositories
  ├── worktrees/       # Git worktrees organized by session/task
  └── logs/            # Instance-specific logs
  ```

### 2. User Interface
- **Primary Interface**: **Web UI** (Responsive design for both Desktop and Mobile).
- **Web UI Layout**:
  - **Sessions (Sidebar)**: Multi-channel organization similar to Slack. Each session represents a separate project or logical work context.
  - **Chat Interface**: Main area for user interaction with the Master Agent (ChatGPT-like experience).
  - **Threads (Sub-Agents)**: Similar to Discord threads, these "hang" under a session. Each thread represents a Claude Code sub-agent task, allowing the user to drill down into specific agent status and history.

### 3. Core Manager
- **Session Isolation**: Each session is isolated using **Git Worktrees**.
- **Repository Management**: 
  - A shared `repos/` directory caches the base git repositories.
  - For each session, CharlieBot creates/manages a dedicated worktree in `worktrees/`, allowing multiple Claude Code instances to work on different branches/PRs simultaneously without file system conflicts.
- **Agent Orchestration**: Master Agent spawns and monitors Worker Claude Code instances within their respective worktree environments.
- **Sub-Agent Monitoring**: Real-time tracking of what Claude Code instances are doing (status, logs, output).

### 4. Integrations
- **GitHub Integration**: Native support for managing Pull Requests (PRs), committing changes, and maintaining task context across different branches/PRs.

### 5. Agent Communication Layer
- **Communication Pattern**:
  - **Master to Worker**: Master Agent spawns Claude Code via shell/process and passes initial instructions via stdin or CLI arguments.
  - **Worker to Master (Progress Tracking)**: 
    - Since Claude Code runs as a separate process, the Master captures stdout/stderr to stream logs to the Web UI.
    - **Status Signaling**: Master monitors file changes and git commits in the session's Worktree to track progress.
    - **Inter-Agent Messaging (Optional)**: If sub-agents need to "ask" the Master for clarification, this will be routed through the core state manager and presented as a notification/thread update in the Web UI for user or Master oversight.

## Technical Stack
- **Language**: Python 3.10+
- **Backend**: 
  - `FastAPI` or `Flask`: To serve the Web UI and API.
  - `asyncio`: For handling concurrent Claude Code instances and real-time updates.
- **Frontend**:
  - `React` or `Next.js`: To build the responsive Web UI.
- **Storage**: `SQLite` or `PostgreSQL` for session and sub-agent history.

## Proposed Workflow
1. **Task Initialization**: 
   - User submits a request via the Web UI.
   - For a **new project**, the Master creates a `project.md` or similar specification.
   - For an **existing project**, the Master identifies the current state from existing documentation/code and updates the task context.
2. **Context Persistence**: 
   - Work is managed through Pull Requests (PRs).
   - The Master Agent tracks the current PR state and ensures sub-agents "inherit" the context of the branch or ongoing work.
3. **Execution**:
   - Master Agent spawns Worker Claude Code instances to perform specific tasks on the codebase.
   - Workers update code and documentation incrementally rather than recreating them.
4. **Review & Summary**:
   - Master summarizes progress back to the Web UI, referencing specific PRs or files changed.

## Initial File Structure
```text
charlie-bot/
├── src/
│   ├── web/            # Web UI and API implementation
│   ├── core/           # Orchestration logic
│   └── agents/         # Agent communication wrappers
├── config/             # Configuration files (default)
├── project.md          # This specification
└── main.py             # Entry point
```

## Next Steps
- [ ] Initialize the project structure and configuration loader.
- [ ] Set up the FastAPI backend and basic Web UI shell.
- [ ] Define the internal API for spawning sub-agents.
- [ ] Create basic task delegation logic.
