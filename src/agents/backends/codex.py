"""CodexBackend — AgentBackend wrapping the `codex exec --json` CLI."""

import asyncio
import json
import shutil
from collections.abc import AsyncIterator
from pathlib import Path
from typing import Awaitable, Callable, Optional

import structlog

from src.agents.backends.base import AgentBackend

log = structlog.get_logger()

_DEFAULT_BUFFER_LIMIT = 1024 * 1024 * 1024  # 1 GB
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

  def __init__(
      self,
      model: str = "gpt-5.3-codex",
      resume_session_id: Optional[str] = None,
      extra_flags: Optional[list[str]] = None,
      buffer_limit: Optional[int] = None,
      on_spawn: Optional[Callable[[int], Awaitable[None]]] = None,
  ):
    self._codex_bin = _resolve_codex_binary()
    self._model = model
    self._resume_session_id = resume_session_id
    self._extra_flags = extra_flags or []
    self._buffer_limit = buffer_limit or _DEFAULT_BUFFER_LIMIT
    self._on_spawn = on_spawn
    self._proc: Optional[asyncio.subprocess.Process] = None
    self.exit_code: int = -1
    self.stderr_text: str = ""
    # Track accumulated text per item_id for delta computation
    self._last_agent_text: dict[str, str] = {}

  @property
  def pid(self) -> Optional[int]:
    return self._proc.pid if self._proc else None

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Spawn codex exec and yield CC-compatible event dicts."""
    codex_env = {**env}
    # Ensure ~/.local/bin is on PATH if codex lives there
    local_bin = str(Path.home() / ".local" / "bin")
    current_path = codex_env.get("PATH", "")
    if local_bin not in current_path.split(":"):
      codex_env["PATH"] = f"{local_bin}:{current_path}"

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
    cmd.append(prompt)

    self._last_agent_text.clear()

    self._proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=codex_env,
        limit=self._buffer_limit,
    )
    if self._on_spawn is not None:
      await self._on_spawn(self._proc.pid)

    assert self._proc.stdout is not None
    async for raw_line in self._proc.stdout:
      line = raw_line.decode("utf-8", errors="replace").strip()
      if not line:
        continue
      try:
        codex_event = json.loads(line)
      except json.JSONDecodeError as e:
        log.debug("codex_line_not_json", error=str(e))
        continue
      for cc_event in self._translate_event(codex_event):
        yield cc_event

    assert self._proc.stderr is not None
    stderr_bytes = await self._proc.stderr.read()
    await self._proc.wait()
    self.exit_code = self._proc.returncode or 0
    self.stderr_text = stderr_bytes.decode("utf-8", errors="replace").strip() if stderr_bytes else ""

  def _translate_event(self, ev: dict) -> list[dict]:
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

  async def terminate(self) -> None:
    """Send SIGTERM; escalate to SIGKILL if process does not exit within 5 s."""
    if self._proc is None or self._proc.returncode is not None:
      return
    try:
      self._proc.terminate()
    except ProcessLookupError:
      log.debug("codex_terminate_pid_gone", pid=self._proc.pid)
      return
    try:
      await asyncio.wait_for(self._proc.wait(), timeout=5.0)
    except asyncio.TimeoutError:
      log.warning("codex_terminate_timeout", pid=self._proc.pid)
      self._proc.kill()
