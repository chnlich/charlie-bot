"""CodexBackend — AgentBackend wrapping the `codex exec --json` CLI."""

import json
from pathlib import Path

import structlog

from src.agents.backends.base import (
    AgentBackend, make_error_event, make_result_event, make_text_event, make_tool_result_event, make_tool_use_event,
    resolve_binary)
from src.core import event_types as ET

log = structlog.get_logger()

# model_reasoning_effort="xhigh" is the only working value.
# Other values are silently ignored by the Codex CLI. Do not make configurable.
_MODEL_REASONING_EFFORT_CONFIG = 'model_reasoning_effort="xhigh"'


class CodexBackend(AgentBackend):
  """Runs a `codex exec --json` subprocess and translates NDJSON events to CC-compatible format."""

  def __init__(self, *, model="gpt-5.3-codex", **kwargs):
    super().__init__(model=model, **kwargs)
    self._codex_bin = resolve_binary("codex", str(Path.home() / ".local" / "bin"))
    # Track accumulated text per item_id for delta computation
    self._last_agent_text: dict[str, str] = {}
    # Track the last rendered todo snapshot to suppress duplicate started/completed payloads.
    self._last_todo_text: dict[str, str] = {}

  def _prepare_cwd(self, cwd: str) -> None:
    """Write AGENTS.md into the cwd so Codex auto-detects it."""
    if not self._instructions_content:
      return
    agents_md = Path(cwd) / 'AGENTS.md'
    agents_md.write_text(self._instructions_content, encoding='utf-8')
    log.debug('codex_wrote_agents_md', path=str(agents_md))

  def _build_command(self, prompt: str) -> list[str]:
    effective_prompt = prompt

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
    self._last_todo_text.clear()
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
          make_result_event(
              input_tokens=usage.get("input_tokens", 0),
              output_tokens=usage.get("output_tokens", 0),
              cache_read=usage.get("cached_input_tokens", 0),
          )
      ]

    # --- turn.failed ---
    if ev_type == "turn.failed":
      error = ev.get("error", {})
      msg = error.get("message") if isinstance(error, dict) else str(error)
      if not msg:
        msg = f"Codex turn.failed with no message. Full event: {json.dumps(ev, default=str)}"
      return [make_error_event(msg)]

    # --- top-level error ---
    if ev_type == "error":
      error = ev.get("error", {})
      msg = error.get("message") if isinstance(error, dict) else str(error)
      if not msg:
        msg = f"Codex error event with no message. Full event: {json.dumps(ev, default=str)}"
      return [make_error_event(msg)]

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
    return [make_text_event(delta)]

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
    return [{"type": ET.THINKING, "content": text}]

  def _handle_command_execution(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "command_execution":
      return []
    if ev.get("type") == "item.started":
      command = item.get("command", "")
      return [make_tool_use_event("Bash", {"command": command})]
    if ev.get("type") == "item.completed":
      output = item.get("output", "")
      return [make_tool_result_event("Bash", output)]
    return []

  def _handle_file_change(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "file_change":
      return []
    filename = item.get("filename", "")
    if not filename:
      return []
    return [{"type": ET.FILE_WRITE, "path": filename}]

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
          log.warning("codex_malformed_tool_args", raw_arguments=arguments)
          arguments = {"raw": arguments}
      return [make_tool_use_event(tool_name, arguments)]
    if ev.get("type") == "item.completed":
      output = item.get("result", item.get("error", ""))
      return [make_tool_result_event(tool_name, str(output))]
    return []

  def _handle_web_search(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "web_search":
      return []
    if ev.get("type") == "item.started":
      query = item.get("query", "")
      return [make_tool_use_event("WebSearch", {"query": query})]
    if ev.get("type") == "item.completed":
      output = item.get("output", "")
      return [make_tool_result_event("WebSearch", str(output))]
    return []

  def _handle_todo_list(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != "todo_list":
      return []
    items = item.get("items", [])
    if not isinstance(items, list):
      return []
    lines = []
    for todo in items:
      if not isinstance(todo, dict):
        continue
      label = self._extract_todo_label(todo)
      if not label:
        continue
      marker = self._todo_marker(todo)
      lines.append(f"- {marker} {label}")
    item_id = item.get("id", "")
    if not lines:
      if item_id:
        self._last_todo_text.pop(item_id, None)
      return []
    text = "\n".join(lines)
    if item_id:
      previous = self._last_todo_text.get(item_id)
      if previous == text:
        return []
      self._last_todo_text[item_id] = text
    return [make_text_event(text)]

  def _extract_todo_label(self, todo: dict) -> str:
    """Return the first non-empty todo label across old and current Codex schemas."""
    for key in ("text", "label", "content", "step"):
      value = todo.get(key, "")
      if isinstance(value, str):
        stripped = value.strip()
        if stripped:
          return stripped
    return ""

  def _todo_marker(self, todo: dict) -> str:
    status = todo.get("status")
    if isinstance(status, str):
      return {"completed": "[x]", "in_progress": "[~]"}.get(status, "[ ]")
    if todo.get("completed") is True:
      return "[x]"
    return "[ ]"

  def _handle_error(self, ev: dict) -> list[dict]:
    item = ev.get("item", {})
    if item.get("type") != ET.ERROR:
      return []
    msg = item.get("message") or f"Codex item error with no message. Full event: {json.dumps(ev, default=str)}"
    return [make_error_event(msg)]
