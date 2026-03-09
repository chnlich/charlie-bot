"""CodexBackend — AgentBackend wrapping the `codex exec --json` CLI."""

import json
import shutil
from pathlib import Path
import structlog

from src.agents.backends.base import AgentBackend

log = structlog.get_logger()

# model_reasoning_effort="xhigh" is the only working value.
# Other values are silently ignored by the Codex CLI. Do not make configurable.
_MODEL_REASONING_EFFORT_CONFIG = 'model_reasoning_effort="xhigh"'


def _resolve_codex_binary() -> str:
  """Resolve the codex binary path, falling back to ~/.local/bin/codex."""
  path = shutil.which("codex")
  if path:
    return path
  fallback = Path.home() / ".local" / "bin" / "codex"
  if fallback.exists():
    return str(fallback)
  raise FileNotFoundError("codex binary not found on PATH or at ~/.local/bin/codex")


class CodexBackend(AgentBackend):
  """Runs a `codex exec --json` subprocess and translates NDJSON events to CC-compatible format."""

  def __init__(self, **kwargs):
    model = kwargs.pop("model", "gpt-5.3-codex")
    instructions_content = kwargs.pop("instructions_content", None)
    resume_session_id = kwargs.pop("resume_session_id", None)
    extra_flags = kwargs.pop("extra_flags", None)
    super().__init__(
        model=model,
        instructions_content=instructions_content,
        resume_session_id=resume_session_id,
        extra_flags=extra_flags,
        **kwargs)
    self._codex_bin = _resolve_codex_binary()
    # Track accumulated text per item_id for delta computation
    self._last_agent_text: dict[str, str] = {}

  def _build_command(self, prompt: str) -> list[str]:
    # Prepend instructions to prompt if provided
    effective_prompt = prompt
    if self._instructions_content:
      effective_prompt = (f"<system-instructions>\n{self._instructions_content}\n</system-instructions>\n\n{prompt}")

    if self._resume_session_id:
      cmd = [
          self._codex_bin,
          "exec",
          "resume",
          "--json",
          "--skip-git-repo-check",
          "--dangerously-bypass-approvals-and-sandbox",
          "--model",
          self._model,
          "--config",
          _MODEL_REASONING_EFFORT_CONFIG,
          self._resume_session_id,
      ]
    else:
      cmd = [
          self._codex_bin,
          "exec",
          "--json",
          "--skip-git-repo-check",
          "--dangerously-bypass-approvals-and-sandbox",
          "--model",
          self._model,
          "--config",
          _MODEL_REASONING_EFFORT_CONFIG,
      ]
    cmd.extend(self._extra_flags)
    cmd.append(effective_prompt)

    self._last_agent_text.clear()
    return cmd

  def _prepare_env(self, env: dict) -> dict:
    codex_env = {**env}
    local_bin = str(Path.home() / ".local" / "bin")
    current_path = codex_env.get("PATH", "")
    if local_bin not in current_path.split(":"):
      codex_env["PATH"] = f"{local_bin}:{current_path}"
    return codex_env

  def translate_event(self, ev: dict) -> list[dict]:
    """Translate a single Codex NDJSON event into CC-compatible event(s)."""
    ev_type = ev.get("type", "")

    # --- thread.started ---
    if ev_type == "thread.started":
      return [{"session_id": ev.get("thread_id", "")}]

    # --- turn.started ---
    if ev_type == "turn.started":
      return []

    # --- turn.completed ---
    if ev_type == "turn.completed":
      usage = ev.get("usage", {})
      return [
          {
              "type": "result",
              "result": "",
              "usage":
                  {
                      "input_tokens": usage.get("input_tokens", 0),
                      "cache_read_input_tokens": usage.get("cached_input_tokens", 0),
                      "cache_creation_input_tokens": 0,
                      "output_tokens": usage.get("output_tokens", 0),
                  },
              "total_cost_usd": 0,
          }
      ]

    # --- turn.failed ---
    if ev_type == "turn.failed":
      error = ev.get("error", {})
      msg = error.get("message", str(error)) if isinstance(error, dict) else str(error)
      return [{"type": "error", "message": msg, "content": msg}]

    # --- top-level error ---
    if ev_type == "error":
      error = ev.get("error", {})
      msg = error.get("message", str(error)) if isinstance(error, dict) else str(error)
      return [{"type": "error", "message": msg, "content": msg}]

    # --- item.started / item.updated / item.completed ---
    if ev_type in ("item.started", "item.updated", "item.completed"):
      return self._translate_item_event(ev)

    log.debug("codex_event_unhandled", type=ev_type)
    return []

  # Handler registry: each handler is called for every item event,
  # preserving multi-fire semantics (independent ifs, not elif).
  _ITEM_HANDLERS = [
      "_handle_agent_message",
      "_handle_reasoning",
      "_handle_command_execution",
      "_handle_file_change",
      "_handle_mcp_tool_call",
      "_handle_web_search",
      "_handle_todo_list",
      "_handle_error",
  ]

  def _translate_item_event(self, ev: dict) -> list[dict]:
    """Translate item.started/updated/completed events."""
    results: list[dict] = []
    for handler_name in self._ITEM_HANDLERS:
      results.extend(getattr(self, handler_name)(ev))
    return results

  def _handle_agent_message(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "agent_message":
      return []
    item_id = item.get("id", "")
    # Newer codex schema emits item.text directly; older schema used content[].
    full_text = item.get("text", "")
    if not full_text:
      content = item.get("content", [])
      full_text = "".join(
          part.get("text", "") for part in content if isinstance(part, dict) and part.get("type") == "text")
    if not full_text:
      return []
    prev = self._last_agent_text.get(item_id, "")
    delta = full_text[len(prev):]
    self._last_agent_text[item_id] = full_text
    if not delta:
      return []
    return [{
        "type": "assistant",
        "message": {
            "content": [{
                "type": "text",
                "text": delta
            }]
        },
    }]

  def _handle_reasoning(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "reasoning":
      return []
    # Newer codex schema emits reasoning text directly; older schema used summary[].
    text = item.get("text", "")
    if not text:
      summary = item.get("summary", [])
      for part in summary:
        if part.get("type") == "summary_text":
          text += part.get("text", "")
    if not text:
      return []
    return [{"type": "thinking", "content": text}]

  def _handle_command_execution(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "command_execution":
      return []
    if ev.get("type") == "item.started":
      command = item.get("command", "")
      return [{"type": "tool_use", "name": "Bash", "input": {"command": command}}]
    if ev.get("type") == "item.completed":
      output = item.get("output", "")
      return [{"type": "tool_result", "tool_name": "Bash", "content": output}]
    return []

  def _handle_file_change(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "file_change":
      return []
    filename = item.get("filename", "")
    if not filename:
      return []
    return [{"type": "file_write", "path": filename}]

  def _handle_mcp_tool_call(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "mcp_tool_call":
      return []
    server = item.get("server_label", "")
    tool = item.get("name", "")
    tool_name = f"mcp:{server}/{tool}" if server else f"mcp:{tool}"
    if ev.get("type") == "item.started":
      arguments = item.get("arguments", {})
      if isinstance(arguments, str):
        try:
          arguments = json.loads(arguments)
        except json.JSONDecodeError:
          arguments = {"raw": arguments}
      return [{"type": "tool_use", "name": tool_name, "input": arguments}]
    if ev.get("type") == "item.completed":
      output = item.get("result", item.get("error", ""))
      return [{"type": "tool_result", "tool_name": tool_name, "content": str(output)}]
    return []

  def _handle_web_search(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "web_search":
      return []
    if ev.get("type") == "item.started":
      query = item.get("query", "")
      return [{"type": "tool_use", "name": "WebSearch", "input": {"query": query}}]
    if ev.get("type") == "item.completed":
      output = item.get("output", "")
      return [{"type": "tool_result", "tool_name": "WebSearch", "content": str(output)}]
    return []

  def _handle_todo_list(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "todo_list":
      return []
    items = item.get("items", [])
    lines = []
    for todo in items:
      status = todo.get("status", "pending")
      label = todo.get("label", todo.get("content", ""))
      marker = {"completed": "[x]", "in_progress": "[~]"}.get(status, "[ ]")
      lines.append(f"- {marker} {label}")
    if not lines:
      return []
    return [{
        "type": "assistant",
        "message": {
            "content": [{
                "type": "text",
                "text": "\n".join(lines)
            }]
        },
    }]

  def _handle_error(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "error":
      return []
    msg = item.get("message", str(item))
    return [{"type": "error", "message": msg, "content": msg}]
