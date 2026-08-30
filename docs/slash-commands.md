# Slash Commands

Slash commands let you run predefined shell scripts or inject canned prompts directly from the chat input. All command processing happens on the backend — just type `/commandname [args]` and press Enter.

---

## Overview

- Type `/` in the chat input to see a popup of available commands.
- Select a command from the popup (click, Tab, or Enter) to fill it in; then add any arguments and press Enter to execute.
- The backend processes the command and either streams a result or dispatches an agent run.
- `/help` is always available — it lists every registered command.
- `Ctrl+/` (or the slash-button beside the chat input) opens the slash sidebar: a form
  rendered from the selected command's `params`, whose values are joined into the
  command's args on Run.

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
    claude_code_flags: ['--flag']  # Optional — extra CLI flags for the Claude Code subprocess
    # --- form fields ---
    params: []                     # Optional — sidebar form fields (see below)
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

## Sidebar form fields (`params`)

`params` is an optional list of form fields for either scope. The slash sidebar
(`Ctrl+/`) renders one input per entry and joins the entered values with spaces;
that joined text becomes the command's args, so templates still substitute only
`{args}` on the backend. A command without `params` runs directly from the sidebar.

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Field key shown to the user when `label` is empty |
| `label` | No | Display label (default: `name`) |
| `type` | No | `text` (default), `number`, `select`, or `checkbox` |
| `default` | No | Initial value; a checkbox checks when it is the string `"true"` |
| `placeholder` | No | Placeholder text for the input |
| `required` | No | Adds a required badge in the form (default: false) |
| `options` | No | Choices for `type: select` |

```yaml
params:
  - {name: environment, label: "Environment", type: select,
     options: [staging, production], required: true}
```

An unchecked checkbox contributes the empty string; a checked one contributes `true`.

---

## Built-in commands

| Command | Description |
|---------|-------------|
| `/help` | Show all available slash commands |
| `/run <task-name>` | Manually trigger a scheduled task |
| `/stop-improve` | Stop an active improve loop after current iteration |

Built-ins are hardcoded in the backend and take precedence over any YAML command
with the same name.

---

## Template variables

| Variable | Available in | Value |
|----------|-------------|-------|
| `{args}` | `shell`, `prompt` | Everything the user typed after the command name |
| `{session_dir}` | `shell` | Absolute path to the current session's data directory |

---

## API reference

### `GET /api/slash/commands`

Returns all registered YAML commands plus the built-ins.

**Response**

```json
[
  { "name": "git", "scope": "shell", "description": "Run git command", "args": "<git args>", "params": [] },
  { "name": "help", "scope": "builtin", "description": "Show available slash commands", "params": [] },
  { "name": "run", "scope": "builtin", "description": "Manually trigger a scheduled task", "args": "<task-name>",
    "params": [ { "name": "task_name", "label": "Task name", "type": "text", "required": true, "placeholder": "e.g. daily-report" } ] },
  { "name": "stop-improve", "scope": "builtin", "description": "Stop an active improve loop after current iteration", "params": [] }
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

**Response — /run task triggered** (HTTP 202)

```json
{ "type": "task_triggered", "task": "daily-report", "session_id": "...", "thread_id": "..." }
```

**Response — /stop-improve**

```json
{ "type": "improve_stopped", "message": "Improve loop will stop after current iteration" }
```

`/run` and `/stop-improve` return `{ "error": "..." }` when the task is unknown, the
scheduler is unavailable, or no improve loop is active in the session.

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
