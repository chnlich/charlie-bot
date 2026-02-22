# Slash Commands

Slash commands let you run predefined shell scripts or inject canned prompts directly from the chat input. All command processing happens on the backend — just type `/commandname [args]` and press Enter.

---

## Overview

- Type `/` in the chat input to see a popup of available commands.
- Select a command from the popup (click, Tab, or Enter) to fill it in; then add any arguments and press Enter to execute.
- The backend processes the command and either streams a result or dispatches an agent run.
- `/help` is always available — it lists every registered command.

---

## Config file

**Location:** `~/.charliebot/slash_commands.yaml`

The file is re-read on every API call — no server restart required when you add or edit commands.

### Full YAML schema

```yaml
commands:
  <command-name>:
    scope: shell | prompt          # Required
    description: "Human-readable description"
    args: "<arg description>"      # Optional — shown in /help popup
    # --- shell fields ---
    command: "shell command {args}" # Required for scope: shell
    cwd: "/path/to/workdir"        # Optional — working directory
    timeout: 10                    # Optional — seconds (default: 10)
    # --- prompt fields ---
    prompt: "Prompt text {args}"   # Required for scope: prompt
```

---

## Scope reference

### `shell` — Run a shell command

Executes the `command` template in a subprocess and returns stdout/stderr synchronously.

| Field | Required | Description |
|-------|----------|-------------|
| `command` | Yes | Shell command string; may contain `{args}` and `{session_dir}` |
| `cwd` | No | Working directory for the subprocess |
| `timeout` | No | Seconds before the process is killed (default: 10) |
| `args` | No | Description string shown in the help popup |

### `prompt` — Inject a prompt into the agent

Substitutes template variables in `prompt` and feeds the result to the master CC agent as a new user message. The response streams via WebSocket exactly like a normal chat message.

| Field | Required | Description |
|-------|----------|-------------|
| `prompt` | Yes | Prompt text; may contain `{args}` |
| `args` | No | Description string shown in the help popup |
| `claude_code_flags` | No | List of extra CLI flags passed to the Claude Code subprocess |

#### `claude_code_flags`

An optional list of CLI flags forwarded directly to the `claude` subprocess when this command runs. Only applies to `scope: prompt`.

```yaml
claude_code_flags: ['--permission-mode', 'plan']
```

Use this to run a command in a restricted permission mode, enable/disable specific tools, or pass any other flag that `claude` accepts.

---

## Built-in commands

| Command | Description |
|---------|-------------|
| `/help` | Show all available slash commands |

`/help` is hardcoded in the backend and cannot be overridden in the YAML file.

---

## Template variables

| Variable | Available in | Value |
|----------|-------------|-------|
| `{args}` | `shell`, `prompt` | Everything the user typed after the command name |
| `{session_dir}` | `shell` | Absolute path to the current session's data directory |

---

## API reference

### `GET /api/slash/commands`

Returns all registered commands plus the built-in `/help`.

**Response**

```json
[
  { "name": "git", "scope": "shell", "description": "Run git command", "args": "<git args>" },
  { "name": "help", "scope": "builtin", "description": "Show available slash commands" }
]
```

---

### `POST /api/slash/{session_id}/execute`

Execute a slash command.

**Request body**

```json
{ "command": "git", "args": "status" }
```

**Response — shell result**

```json
{
  "type": "shell_result",
  "command": "git",
  "stdout": "On branch main\n...",
  "stderr": "",
  "exit_code": 0
}
```

**Response — prompt dispatched** (HTTP 202)

```json
{ "type": "prompt_dispatched", "command": "summarize" }
```

**Response — /help**

```json
{ "type": "help", "commands": [ ... ] }
```

**Response — unknown command**

```json
{ "error": "Unknown command: /foo" }
```

---

## Examples

### Adding a git status command

```yaml
commands:
  git:
    scope: shell
    description: "Run a git command"
    args: "<git args>"
    command: "git {args}"
    cwd: "/path/to/your/project"
    timeout: 15
```

Usage: `/git log --oneline -5`

---

### Adding a summarize prompt command

```yaml
commands:
  summarize:
    scope: prompt
    description: "Summarize the conversation"
    prompt: "Please summarize our conversation so far in concise bullet points."
```

Usage: `/summarize`

---

### Shell command with session directory

```yaml
commands:
  ls-uploads:
    scope: shell
    description: "List uploaded files for this session"
    command: "ls -lh {session_dir}/uploads 2>/dev/null || echo 'No uploads yet'"
    timeout: 5
```

Usage: `/ls-uploads`
