# Project Spec: CharlieBot

## Project Overview
**CharlieBot** is a Python-based system designed to coordinate and manage multiple **Claude Code** instances to complete complex tasks. The primary interface for interaction is a responsive Web UI for desktop and mobile access.

## Objectives
- **Agent Orchestration**: Enable an API-based **Master Agent** (e.g., Gemini 3 Flash, Kimi k2.5) to manage and coordinate **Claude Code** Worker instances.
- **Task Delegation**: Master delegates coding tasks to Claude Code Workers; actual code analysis and implementation planning is performed by the Workers themselves.
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
  ├── data/            # JSON persistence (session history, agent states)
  ├── repos/           # Shared storage for base git repositories
  ├── worktrees/       # Git worktrees organized by session
  └── logs/            # Instance-specific logs
  ```
- **Repository Code Structure** (Stateless, shared across instances):
  ```text
  charlie-bot/
  ├── src/
  │   ├── web/            # FastAPI backend and API
  │   ├── core/           # Orchestration logic
  │   └── agents/         # Agent communication wrappers
  ├── frontend/           # React SPA source (development only)
  ├── static/             # React build output (served by FastAPI)
  ├── config/             # Default configuration templates (examples only)
  ├── project.md          # This specification
  └── server.py           # Entry point
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
- **Session Isolation**: Each session is isolated using **Git Worktrees**. Each session gets its own dedicated worktree directory under `~/.charliebot/worktrees/{session_id}/`.
- **Session Data Model**: 
  - Each Session corresponds to one Git Worktree (associated with a specific repository and branch).
  - Sessions are user-named for easy identification in the sidebar.
  - Session configuration (which repo, which branch) is stored in JSON.
- **Repository Management**: 
  - A shared `repos/` directory caches the base git repositories (bare clones).
  - For each session, CharlieBot creates/manages a dedicated worktree in `worktrees/{session_id}/`, allowing multiple sessions to work on different branches simultaneously without file system conflicts.
- **Agent Architecture**:
  - **Master Agent**: An API-based LLM (e.g., Gemini 3 Flash, Kimi k2.5) responsible for **managing and coordinating** Claude Code Workers. The Master decides when to spawn Workers and monitors their completion, but does not perform coding analysis or create implementation plans.
  - **Worker Agent**: Always **Claude Code**. The Worker performs actual coding tasks: analyzing requirements, planning implementation, editing files, git operations, and testing within its designated worktree. Claude Code should run in **YOLO mode** (or equivalent flag) to allow all operations without interactive confirmation, enabling fully automated task execution.
- **Concurrent Workers**: A single Session can run **multiple Workers concurrently** (each Worker is a separate Thread under the Session). The Session decides how to orchestrate these Workers (e.g., parallel tasks, sequential steps).
- **Sub-Agent Monitoring**: Real-time tracking of what Claude Code instances are doing (status, logs, output).

### 4. Agent Communication & State Management
- **Master-Agent Communication**:
  - The Master (API-based LLM) receives user requests and generates instructions for Workers.
  - Master maintains conversation history and task state in **JSON files** (persisted to `~/.charliebot/data/`).
- **Real-Time Worker Output Streaming**:
  - For **real-time tracking** of Claude Code terminal output, the backend uses **WebSockets** or **Server-Sent Events (SSE)** to stream the PTY output directly from the Worker process to the frontend.
  - This provides smooth, low-latency log viewing without polling overhead.
- **Worker State Persistence**:
  - Worker (Claude Code) state (logs, status, progress) is persisted to the **file system** (disk) in real-time by the supervisor process.
  - **Work Process Persistence**: The entire sub-agent work process (task instructions, intermediate outputs, final results, git commits) is persisted to disk. If the Master is restarted, it can **reload previous results** and resume the workflow from where it left off without losing context.
  - **Completion Notification**: When a Worker (sub-agent) finishes its task, the **Master session that triggered it must be notified**. The Master can then review the results and continue the workflow (e.g., spawn additional Workers, summarize to user, or ask for clarification).
  - **History Retrieval**: When loading a Session/Thread history (not real-time), the frontend may use **HTTP GET requests** to fetch past logs from the persisted JSON files.
  - **Benefits**: Combines real-time streaming (WebSocket/SSE) with durable persistence (JSON files), ensuring both live monitoring and crash recovery capabilities.

## Technical Stack
- **Language**: Python 3.10+
- **Master Agent**: API-based LLM (Gemini 3 Flash, Kimi k2.5, etc.)
- **Worker Agent**: Claude Code (local CLI invocation)
- **Backend**: 
  - `FastAPI` or `Flask`: To serve the Web UI and API.
  - `WebSockets` or `SSE` (Server-Sent Events): For real-time streaming of Claude Code terminal output to the frontend.
  - `asyncio`: For handling concurrent Claude Code instances and real-time updates.
- **Frontend**:
  - **React SPA** (Single Page Application) built to static files.
  - **FastAPI StaticFiles**: The React build output (`dist/` or `build/`) is served as static files by FastAPI.
  - **Benefits**: Keeps the entire runtime Python-Native; users don't need Node.js to run CharlieBot, only for development.
- **Storage**:
  - **Agent Output**: JSON files for sub-agent logs, results, and state.
  - **Configuration**: YAML files for manual user configuration.

## Proposed Workflow
1. **Task Initialization**: 
   - User submits a request via the Web UI (text or voice).
   - The **Master Agent** (API-based LLM) receives the request and determines if a Claude Code Worker is needed.
2. **Worker Delegation**:
   - If coding work is required, the Master spawns a **Claude Code Worker** instance in the appropriate Git Worktree.
   - The Worker (Claude Code) analyzes the task, plans the implementation, and executes the coding work.
   - The Master monitors Worker status but does not perform the actual coding analysis.
3. **Execution & Persistence**:
   - Workers operate within their designated Git Worktrees and persist all outputs to disk.
   - If the Master restarts, it reloads previous Worker results from disk.
4. **Completion & Continuation**:
   - When a Worker finishes, the Master is notified and can decide next steps (spawn more Workers, summarize to user, etc.).
