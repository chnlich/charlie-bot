# Project Spec: CharlieBot

## Project Overview
**CharlieBot** is a Python-based system designed to coordinate and manage multiple **Claude Code** instances to complete complex tasks. The primary interface for interaction is a responsive Web UI for desktop and mobile access.

## Objectives
- **Agent Orchestration**: Enable an API-based **Master Agent** (e.g., Gemini 3 Flash, Kimi k2.5) to plan tasks and spawn **Claude Code** Worker instances to execute them.
- **Task Delegation**: Master breaks down complex user requests and delegates coding sub-tasks to specialized Claude Code Workers.
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
- **Voice Input (Hard Requirement)**: 
  - **Interaction Pattern**: **Push-to-Talk / Toggle-to-Talk** (Press/click button to start recording, press/click again to stop and send).
  - **Transcription Workflow**:
    1. Audio file is uploaded to the backend.
    2. Backend uses **Gemini 3 Flash** for transcription, supporting **Chinese, English, or mixed** (user speaks both languages).
    3. The **transcribed message is displayed** in the chat UI before any further processing.
    4. The transcription is then passed to the Master Agent with a system prompt indicating: *"This is a voice-transcribed message and may not be exactly accurate. Please ask clarifying questions if anything is unclear."*
  - **Language Support**: Gemini 3 Flash handles code-switching between Chinese and English naturally.
- **Web UI Layout**:
  - **Sessions (Sidebar)**: Multi-channel organization similar to Slack. Each session represents a separate project or logical work context.
  - **Chat Interface**: Main area for user interaction with the Master Agent (ChatGPT-like experience).
  - **Threads (Sub-Agents)**: Similar to Discord threads, these "hang" under a session. Each thread represents a Claude Code sub-agent task, allowing the user to drill down into specific agent status and history.

### 3. Core Manager
- **Session Isolation**: Each session is isolated using **Git Worktrees**.
- **Repository Management**: 
  - A shared `repos/` directory caches the base git repositories.
  - For each session, CharlieBot creates/manages a dedicated worktree in `worktrees/`, allowing multiple Claude Code instances to work on different branches/PRs simultaneously without file system conflicts.
- **Agent Architecture**:
  - **Master Agent**: An API-based LLM (e.g., Gemini 3 Flash, Kimi k2.5) responsible for task planning, decision making, and coordinating workflows. The Master does not directly edit code; it orchestrates.
  - **Worker Agent**: Always **Claude Code**. The Worker is spawned by the Master to perform actual coding tasks (file edits, git operations, testing) within its designated worktree.
- **Sub-Agent Monitoring**: Real-time tracking of what Claude Code instances are doing (status, logs, output).

### 4. Integrations
- **GitHub Integration**: Native support for managing Pull Requests (PRs), committing changes, and maintaining task context across different branches/PRs.

### 5. Agent Communication & State Management
- **Master-Agent Communication**:
  - The Master (API-based LLM) receives user requests and generates instructions for Workers.
  - Master maintains conversation history and task state in the database.
- **Worker State Persistence**:
  - Worker (Claude Code) state (logs, status, progress) is persisted to the **file system** (disk) in real-time by the supervisor process.
  - **Web UI Interaction**: When the user accesses a Session/Thread, the frontend issues **HTTP GET requests**.
  - **Backend Logic**: Upon receiving a GET request, CharlieBot queries the file system/database to retrieve the latest logs and state for that specific Worker.
  - **Benefits**: Reduces overhead, ensures data persistence across restarts, and simplifies the communication architecture to a standard Request-Response model.

## Technical Stack
- **Language**: Python 3.10+
- **Master Agent**: API-based LLM (Gemini 3 Flash, Kimi k2.5, etc.)
- **Worker Agent**: Claude Code (local CLI invocation)
- **Backend**: 
  - `FastAPI` or `Flask`: To serve the Web UI and API.
  - `asyncio`: For handling concurrent Claude Code instances and real-time updates.
- **Frontend**:
  - `React` or `Next.js`: To build the responsive Web UI.
- **Storage**: `SQLite` or `PostgreSQL` for session and sub-agent history.

## Proposed Workflow
1. **Task Initialization**: 
   - User submits a request via the Web UI (text or voice).
   - The **Master Agent** (API-based LLM) analyzes the request and creates a task plan.
2. **Context Persistence**: 
   - Work is managed through Pull Requests (PRs).
   - The Master tracks the current PR state and ensures Workers inherit the context of the branch.
3. **Execution**:
   - Master spawns **Claude Code Worker** instances via shell to perform specific tasks on the codebase.
   - Workers operate within their designated Git Worktrees.
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
