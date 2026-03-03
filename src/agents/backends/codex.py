"""CodexBackend — AgentBackend wrapping the `codex exec --json` CLI."""

import json
import shutil
from pathlib import Path
from typing import Optional

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
        model=model, instructions_content=instructions_content,
        resume_session_id=resume_session_id, extra_flags=extra_flags, **kwargs)
    self._codex_bin = _resolve_codex_binary()
    # Track accumulated text per item_id for delta computation
    self._last_agent_text: dict[str, str] = {}

  def _build_command(self, prompt: str) -> list[str]:
    # Prepend instructions to prompt if provided
    effective_prompt = prompt
    if self._instructions_content:
      effective_prompt = (
          f"<system-instructions>\n{self._instructions_content}\n</system-instructions>\n\n{prompt}")

    if self._resume_session_id:
      cmd = [
          self._codex_bin, "exec", "resume", "--json", "--skip-git-repo-check",
          "--dangerously-bypass-approvals-and-sandbox",
          "--model", self._model,
          "--config", _MODEL_REASONING_EFFORT_CONFIG,
          self._resume_session_id,
      ]
    else:
      cmd = [
          self._codex_bin, "exec", "--json", "--skip-git-repo-check",
          "--dangerously-bypass-approvals-and-sandbox",
          "--model", self._model,
          "--config", _MODEL_REASONING_EFFORT_CONFIG,
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
      return [{
          "type": "result",
          "result": "",
          "usage": {
              "input_tokens": usage.get("input_tokens", 0),
              "cache_read_input_tokens": usage.get("cached_input_tokens", 0),
              "cache_creation_input_tokens": 0,
              "output_tokens": usage.get("output_tokens", 0),
          },
          "total_cost_usd": 0,
      }]

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

  def _translate_item_event(self, ev: dict) -> list[dict]:
    """Translate item.started/updated/completed events."""
    results: list[dict] = []
    item = ev.get("item", {})
    item_type = item.get("type", "")
    item_id = item.get("id", "")

    # --- agent_message ---
    if item_type == "agent_message":
      # Newer codex schema emits item.text directly; older schema used content[].
      full_text = item.get("text", "")
      if not full_text:
        content = item.get("content", [])
        full_text = "".join(
            part.get("text", "")
            for part in content
            if isinstance(part, dict) and part.get("type") == "text")
      if full_text:
        prev = self._last_agent_text.get(item_id, "")
        delta = full_text[len(prev):]
        if delta:
          results.append({
              "type": "assistant",
              "message": {"content": [{"type": "text", "text": delta}]},
          })
        self._last_agent_text[item_id] = full_text

    # --- reasoning ---
    if item_type == "reasoning":
      # Newer codex schema emits reasoning text directly; older schema used summary[].
      text = item.get("text", "")
      if not text:
        summary = item.get("summary", [])
        for part in summary:
          if part.get("type") == "summary_text":
            text += part.get("text", "")
      if text:
        results.append({"type": "thinking", "content": text})

    # --- command_execution ---
    if item_type == "command_execution":
      if ev.get("type") == "item.started":
        command = item.get("command", "")
        results.append({"type": "tool_use", "name": "Bash", "input": {"command": command}})
      elif ev.get("type") == "item.completed":
        output = item.get("output", "")
        results.append({"type": "tool_result", "tool_name": "Bash", "content": output})

    # --- file_change ---
    if item_type == "file_change":
      filename = item.get("filename", "")
      if filename:
        results.append({"type": "file_write", "path": filename})

    # --- mcp_tool_call ---
    if item_type == "mcp_tool_call":
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
        results.append({"type": "tool_use", "name": tool_name, "input": arguments})
      elif ev.get("type") == "item.completed":
        output = item.get("result", item.get("error", ""))
        results.append({"type": "tool_result", "tool_name": tool_name, "content": str(output)})

    # --- web_search ---
    if item_type == "web_search":
      if ev.get("type") == "item.started":
        query = item.get("query", "")
        results.append({"type": "tool_use", "name": "WebSearch", "input": {"query": query}})
      elif ev.get("type") == "item.completed":
        output = item.get("output", "")
        results.append({"type": "tool_result", "tool_name": "WebSearch", "content": str(output)})

    # --- todo_list ---
    if item_type == "todo_list":
      items = item.get("items", [])
      lines = []
      for todo in items:
        status = todo.get("status", "pending")
        label = todo.get("label", todo.get("content", ""))
        marker = {"completed": "[x]", "in_progress": "[~]"}.get(status, "[ ]")
        lines.append(f"- {marker} {label}")
      if lines:
        results.append({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "\n".join(lines)}]},
        })

    # --- error ---
    if item_type == "error":
      msg = item.get("message", str(item))
      results.append({"type": "error", "message": msg, "content": msg})

    return results
