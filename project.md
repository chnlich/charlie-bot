# Project Spec: CharlieBot

## Project Overview
**CharlieBot** is a Python-based system designed to coordinate and manage multiple AI agents (specifically Claude Code/OpenClaw instances) to complete complex tasks. The primary interface for interaction is a responsive Web UI with Discord integration for notifications.

## Objectives
- **Agent Orchestration**: Enable a "Master" agent to spawn, monitor, and manage "Worker" agents.
- **Discord Integration**: Provide a robust Discord Bot interface for users to submit tasks and receive updates.
- **Task Delegation**: Break down complex user requests into sub-tasks assigned to specialized sub-agents.
- **Python-Native**: Built entirely in Python for extensibility and ease of integration with AI toolsets.

## Key Design Principles

### 1. Separation of Code and Configuration
- **Stateless Application**: The core logic remains in the installation/repository directory and is intended to be shared across multiple instances.
- **Per-Instance Configuration**: All instance-specific data (database, configs, logs, session state) is stored in a dedicated user-directory, following the pattern of tools like OpenClaw.
- **Home Directory Path**: Default path will be `~/.charliebot/` (or configurable via environment variable `CHARLIEBOT_HOME`).
- **Structure of Home Directory**:
  ```text
  ~/.charliebot/
  ├── config.yaml      # Instance-specific configuration (API keys, bot tokens)
  ├── data/            # Persistence (SQLite database)
  └── logs/            # Instance-specific logs
  ```

### 2. User Interface
- **Primary Interface**: **Web UI** (Responsive design for both Desktop and Mobile).
- **Secondary Interface**: Discord Bot (for notifications and quick commands).
- **Web UI Layout**:
  - **Sessions (Sidebar)**: Multi-channel organization similar to Slack/Discord. Each session represents a separate project or logical work context for isolation.
  - **Chat Interface**: Main area for user interaction with the Master Agent (ChatGPT-like experience).
  - **Sub-Agent Tracking (Threads)**: Visualization of sub-agent status and history, nested within the session (e.g., as sidebar threads or detail views).

### 2. Core Manager
- **Session Isolation**: Ensuring task state and file context are isolated per session.
- **Agent Orchestration**: Master Agent spawns and monitors Worker Agents.
- **Sub-Agent Monitoring**: Real-time tracking of what sub-agents are doing (status, logs, output).

### 3. Agent Communication Layer
- **Interface**: Protocol for the Master to send instructions and receive results from sub-agents.
- **Context Management**: Passing relevant history and file context between agents.

## Technical Stack
- **Language**: Python 3.10+
- **Backend**: 
  - `FastAPI` or `Flask`: To serve the Web UI and API.
  - `discord.py`: For Discord bot integration.
  - `asyncio`: For handling concurrent agents and real-time updates.
- **Frontend**:
  - `React` or `Next.js` (or a Python-based alternative like `Streamlit`/`NiceGUI` for rapid development): To build the responsive Web UI.
- **Storage**: `SQLite` or `PostgreSQL` for session and sub-agent history.

## Proposed Workflow
1. **Task Initialization**: 
   - User submits a request via Discord.
   - For a **new project**, the Master creates a `project.md` or similar specification.
   - For an **existing project**, the Master identifies the current state from existing documentation/code and updates the task context.
2. **Context Persistence**: 
   - Work is managed through Pull Requests (PRs).
   - The Master Agent tracks the current PR state and ensures sub-agents "inherit" the context of the branch or ongoing work.
3. **Execution**:
   - Master Agent spawns Worker Agents to perform specific tasks on the codebase.
   - Workers update code and documentation incrementally rather than recreating them.
4. **Review & Summary**:
   - Master summarizes progress back to Discord, referencing specific PRs or files changed.

## Initial File Structure
```text
claude-code-manager/
├── src/
│   ├── bot/            # Discord bot implementation
│   ├── core/           # Orchestration logic
│   └── agents/         # Agent communication wrappers
├── config/             # Configuration files
├── project.md          # This specification
└── main.py             # Entry point
```

## Next Steps
- [ ] Initialize Discord Bot boilerplate.
- [ ] Define the internal API for spawning sub-agents.
- [ ] Create basic task delegation logic.
