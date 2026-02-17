# Project Spec: Claude Code Manager

## Project Overview
The **Claude Code Manager** is a Python-based system designed to coordinate and manage multiple AI agents (specifically Claude Code/OpenClaw instances) to complete complex tasks. The primary interface for interaction is Discord, allowing for a structured, conversational management environment.

## Objectives
- **Agent Orchestration**: Enable a "Master" agent to spawn, monitor, and manage "Worker" agents.
- **Discord Integration**: Provide a robust Discord Bot interface for users to submit tasks and receive updates.
- **Task Delegation**: Break down complex user requests into sub-tasks assigned to specialized sub-agents.
- **Python-Native**: Built entirely in Python for extensibility and ease of integration with AI toolsets.

## Key Components

### 1. Discord Interface (Bot)
- **Library**: `discord.py`
- **Features**:
  - Command handling for task submission.
  - Status updates and notifications via Discord threads/channels.
  - Interactive buttons/modals for task approval or feedback.

### 2. Core Manager
- **Task Dispatcher**: Logic to parse user intent and determine if a sub-agent is needed.
- **Process Management**: Monitoring the lifecycle of spawned agent processes.
- **State Store**: Keeping track of active tasks, agent assignments, and progress.

### 3. Agent Communication Layer
- **Interface**: Protocol for the Master to send instructions and receive results from sub-agents.
- **Context Management**: Passing relevant history and file context between agents.

## Technical Stack
- **Language**: Python 3.10+
- **Primary Libraries**:
  - `discord.py`: For Discord bot functionality.
  - `pydantic`: For data validation and configuration.
  - `asyncio`: For handling concurrent agent tasks and bot events.
  - `OpenClaw/Claude Code API`: (Assuming integration via local CLI or API).

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
