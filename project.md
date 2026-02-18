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
  ├── MEMORY.md        # Globally shared memory across all sessions (user preferences, facts)
  ├── PAST_TASKS.md    # Global record of all completed tasks across all sessions
  ├── data/            # JSON persistence (session history, agent states)
  ├── repos/           # Shared storage for base git repositories
  ├── worktrees/       # Git worktrees organized by session
  └── logs/            # Instance-specific logs
  ```
- **Global Memory (MEMORY.md)**: A single `MEMORY.md` file shared across all sessions. The Master Agent reads this file at the start of each conversation and updates it whenever:
  - The user expresses a preference (e.g., "I prefer dark mode", "Always use spaces instead of tabs")
  - New facts about the user are revealed (e.g., "I work at Citadel", "My favorite editor is Vim")
  - Important context that should persist across sessions is identified
  - This ensures continuity and personalization across all sessions, regardless of which project the user is working on.
- **Global Task History (PAST_TASKS.md)**: A single `PAST_TASKS.md` file at the global level, shared across all sessions:
  - Records all completed tasks across all projects/sessions with detailed summaries
  - Includes: task description, session context, approach taken, files modified, issues encountered, solutions applied
  - **On-Demand Access**: Due to its potentially large size, Master Agent **does not read this file at the start of each conversation**. Instead, it serves as a searchable archive — Master queries it on-demand when historical context is needed (e.g., via keyword search or semantic retrieval).
  - **Concurrency Guard**: File access must be synchronized to prevent race conditions when multiple Workers or the Master attempt to update the file simultaneously (e.g., file locking or atomic writes).
- **Concurrent Worker Strategy & Branch Isolation**:
  - **Default Policy**: Master employs a **concurrent Worker strategy** — multiple related tasks are executed in parallel by spawning multiple Workers (Threads) under the same Session.
  - **Branch Isolation**: Since all Workers in a Session share the same worktree directory, each Worker must operate on its own **isolated Git branch** to prevent file conflicts:
    - Master creates a unique branch for each Worker (e.g., `charliebot/task-{timestamp}-{task-id}`)
    - Worker performs all edits, commits, and operations on its dedicated branch
    - After completion, Master decides whether to merge, rebase, or keep the branch separate based on task outcome
  - **Benefits**: Maximizes throughput for independent tasks while maintaining isolation; Master coordinates branch lifecycle (creation, merge, cleanup).
- **Worker Instructions (CLAUDE.md)**:
  - **Default Shared Instructions**: A default instruction template is stored in the repository (`config/claude-default.md`) containing general guidelines for all Claude Code Workers (e.g., coding standards, YOLO mode behavior, git commit conventions).
  - **Per-Task Instructions**: Each time the Master spawns a Worker, it creates a `CLAUDE.md` file in the session's worktree directory containing:
    - The default shared instructions (prepended)
    - Specific task description and objectives
    - Any session-specific context or constraints
    - References to relevant files or previous work (from PAST_TASKS.md)
  - **Workflow**: Claude Code reads `CLAUDE.md` at startup to understand the task context before execution.
- **Repository Code Structure** (Stateless, shared across instances):
  ```text
  charlie-bot/
  ├── src/                # All Python backend code
  │   ├── api/            # FastAPI backend and API
  │   ├── core/           # Orchestration logic
  │   └── agents/         # Agent communication wrappers
  ├── web/                # All frontend-related code
  │   ├── src/            # React SPA source (Node.js required for development only)
  │   └── static/         # React build output (served by FastAPI at runtime)
  ├── config/             # Default configuration templates (examples only)
  ├── .yapf               # YAPF code style configuration
  ├── server.py           # Entry point
  └── project.md          # This specification
  ```

## Development Guidelines

### Code Style
- **All Languages**: Follow **Google Code Style**.
- **Python**: Enforced via YAPF with the following configuration (`.yapf`):
  ```ini
  [style]
  based_on_style = google
  indent_width = 2
  split_before_first_argument = true
  column_limit = 120
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
   - If coding work is required, the Master analyzes task dependencies and may spawn **one or more Claude Code Worker instances** concurrently.
   - For concurrent Workers within the same Session, Master creates **isolated Git branches** for each Worker to prevent conflicts.
   - Each Worker (Claude Code) analyzes its assigned task, plans the implementation, and executes on its dedicated branch.
   - The Master monitors all Worker statuses but does not perform the actual coding analysis.
3. **Execution & Persistence**:
   - Workers operate within their designated Git Worktrees and persist all outputs to disk.
   - If the Master restarts, it reloads previous Worker results from disk.
4. **Completion & Continuation**:
   - When a Worker finishes, the Master is notified and can decide next steps (spawn more Workers, summarize to user, etc.).

## Error Handling & Resilience
- **Master Agent Fallback**:
  - The Master Agent uses **Gemini 3 Flash** as the primary model.
  - If Gemini 3 Flash errors out (API failure, rate limit, etc.), the system automatically **switches to Kimi k2.5** as the fallback model.
  - The switch is transparent to the user and the conversation context is preserved.
  - **Model Abstraction**: The LLM client is wrapped in an abstract class/interface (e.g., `LLMProvider`), making it straightforward to add or switch to other models (OpenAI, Anthropic, local models, etc.) without modifying core logic.
- **Quota Exhaustion Handling**:
  - If Claude Code returns a quota-exhausted error, the Worker is paused and its task enters a **PENDING_QUOTA** state.
  - The Master session periodically checks (polls) whether the quota has recovered.
  - Once quota is available, the Master automatically resumes the queued task(s) without user intervention.
  - The user is notified when a task is resumed after quota recovery.
  - All task context is persisted to disk, ensuring no progress is lost during the waiting period.
