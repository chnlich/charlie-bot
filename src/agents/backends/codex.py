"""CodexBackend — AgentBackend wrapping the `codex exec --json` CLI."""

import asyncio
import json
import os
from pathlib import Path
from typing import ClassVar

import structlog

from src.agents.backends.base import (
  USER_LOCAL_BIN,
  AgentBackend,
  iter_ndjson_events,
  make_error_event,
  make_result_event,
  make_text_event,
  make_tool_result_event,
  make_tool_use_event,
  prepend_path_dir,
  resolve_binary,
)
from src.core import event_types as ET
from src.core.codex_pricing import calculate_codex_usage_cost_usd
from src.core.process import wait_or_kill_group

log = structlog.get_logger()

_MAX_ONE_SHOT_STDERR_BYTES = 4 * 1024


class CodexBackend(AgentBackend):
  """Runs a `codex exec --json` subprocess and translates NDJSON events to CC-compatible format."""

  def __init__(self, *, model: str, codex_home: str | None = None,
               model_reasoning_effort: str | None = None,
               model_auto_compact_token_limit: int | None = None, **kwargs):
    if not model:
      raise ValueError("codex backend requires a model (set backend_options[].model in config.yaml)")
    super().__init__(model=model, **kwargs)
    self._codex_bin = resolve_binary("codex", USER_LOCAL_BIN)
    self._codex_home = str(Path(codex_home).expanduser()) if codex_home else None
    self._model_reasoning_effort = "xhigh" if model_reasoning_effort is None else model_reasoning_effort
    self._model_auto_compact_token_limit = model_auto_compact_token_limit
    # Track accumulated text per item_id for delta computation
    self._last_agent_text: dict[str, str] = {}
    # Track accumulated reasoning text per item_id for delta computation.
    self._last_reasoning_text: dict[str, str] = {}
    # Track the last rendered todo snapshot to suppress duplicate started/completed payloads.
    self._last_todo_text: dict[str, str] = {}

  def _prepare_cwd(self, cwd: str) -> None:
    """Write AGENTS.md into the cwd so Codex auto-detects it."""
    self._write_instructions_file(cwd, 'AGENTS.md', 'codex_wrote_agents_md')

  def _model_config_args(self) -> list[str]:
    args = ["--config", f'model_reasoning_effort="{self._model_reasoning_effort}"']
    if self._model_auto_compact_token_limit is not None:
      args.append("--config")
      args.append(f"model_auto_compact_token_limit={self._model_auto_compact_token_limit}")
    return args

  def _exec_args(self) -> list[str]:
    """argv shared by streaming, resume, and one-shot `codex exec` runs."""
    return [
        "--json",
        "--skip-git-repo-check",
        "--dangerously-bypass-approvals-and-sandbox",
        "--model",
        self._model,
        *self._model_config_args(),
    ]

  def _build_command(self, prompt: str) -> list[str]:
    effective_prompt = prompt

    if self._resume_session_id:
      cmd = [
          self._codex_bin,
          "exec",
          "resume",
          *self._exec_args(),
          self._resume_session_id,
      ]
    else:
      cmd = [
          self._codex_bin,
          "exec",
          *self._exec_args(),
      ]
    cmd.extend(self._extra_flags)
    cmd.extend(["--", effective_prompt])

    self._last_agent_text.clear()
    self._last_reasoning_text.clear()
    self._last_todo_text.clear()
    return cmd

  def _prepare_env(self, env: dict) -> dict:
    codex_env = {**env}
    prepend_path_dir(codex_env, USER_LOCAL_BIN)
    if self._codex_home:
      codex_env['CODEX_HOME'] = self._codex_home
    return codex_env

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float) -> str:
    """Generate text via `codex exec --json` with the configured model and effort.

    Codex has no system-prompt flag, so the system prompt is framed into the user
    prompt. agent_message text is accumulated with the same cumulative-delta logic
    as the streaming path (_handle_agent_message). The process group is killed on
    timeout. Structured backend failures are raised instead of being mistaken for
    non-JSON assistant text.
    """
    from src.core.message_aggregator import extract_text_from_message

    framed = self._frame_system_prompt(system_prompt, prompt)
    cmd = [
        self._codex_bin,
        "exec",
        *self._exec_args(),
        "--",
        framed,
    ]
    self._last_agent_text.clear()
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=self._prepare_env(dict(os.environ)),
        limit=self._buffer_limit,
        start_new_session=True,
    )

    async def _read_bounded_stderr() -> bytes:
      assert proc.stderr is not None
      captured = bytearray()
      while True:
        chunk = await proc.stderr.read(8192)
        if not chunk:
          break
        if len(captured) < _MAX_ONE_SHOT_STDERR_BYTES:
          remaining = _MAX_ONE_SHOT_STDERR_BYTES - len(captured)
          captured.extend(chunk[:remaining])
      return bytes(captured)

    async def _collect() -> tuple[str, str | None, int | None]:
      parts: list[str] = []
      structured_error: str | None = None
      assert proc.stdout is not None
      async for ev in iter_ndjson_events(proc.stdout):
        for translated in self.translate_event(ev):
          translated_type = translated.get("type")
          if translated_type == ET.ERROR:
            message = translated.get("message") or translated.get("content")
            if message and structured_error is None:
              structured_error = str(message).strip()
            continue
          if translated_type == ET.ASSISTANT and ev.get("item", {}).get("type") == "agent_message":
            parts.append(extract_text_from_message(translated.get("message")))
      wait_result = await proc.wait()
      returncode = proc.returncode if isinstance(proc.returncode, int) else wait_result
      return "".join(parts).strip(), structured_error, returncode

    stderr_task = asyncio.create_task(_read_bounded_stderr())

    async def _run() -> tuple[str, str | None, int | None, bytes]:
      text, structured_error, returncode = await _collect()
      stderr = await stderr_task
      return text, structured_error, returncode, stderr

    text, structured_error, returncode, stderr = await wait_or_kill_group(_run(), timeout, proc.pid, stderr_task)
    if structured_error:
      raise RuntimeError(f"Codex one-shot failed: {structured_error}")

    stderr_text = stderr.decode("utf-8", errors="replace").strip()
    if returncode != 0:
      detail = f": {stderr_text}" if stderr_text else ""
      raise RuntimeError(f"Codex one-shot failed (exit {returncode}){detail}")
    if not text:
      detail = f": {stderr_text}" if stderr_text else ""
      raise RuntimeError(f"Codex one-shot returned no assistant text{detail}")
    return text

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
      cost = calculate_codex_usage_cost_usd(self._model, usage)
      return [
          make_result_event(
              input_tokens=usage.get("input_tokens", 0),
              output_tokens=usage.get("output_tokens", 0),
              cache_read=usage.get("cached_input_tokens", 0),
              cost=cost,
          )
      ]

    # --- turn.failed / top-level error ---
    if ev_type in ("turn.failed", "error"):
      error = ev.get("error", {})
      msg = error.get("message") if isinstance(error, dict) else str(error)
      if not msg:
        msg = ev.get("message")
      if not msg:
        msg = f"Codex {ev_type} with no message. Full event: {json.dumps(ev, default=str)}"
      return [make_error_event(msg)]

    # --- item.started / item.updated / item.completed ---
    if ev_type in ("item.started", "item.updated", "item.completed"):
      return self._translate_item_event(ev)

    log.debug("codex_event_unhandled", type=ev_type)
    return []

  # Handler registry: each handler is called for every item event,
  # preserving multi-fire semantics (independent ifs, not elif).
  _ITEM_HANDLERS: ClassVar[list[str]] = [
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
    item_id = item.get("id", "")
    prev = self._last_reasoning_text.get(item_id, "")
    delta = text[len(prev):]
    self._last_reasoning_text[item_id] = text
    if not delta:
      return []
    return [{"type": ET.THINKING, "content": delta}]

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
    if ev.get("type") != "item.completed":
      return []
    item = ev.get("item", {})
    if item.get("type") != "file_change":
      return []
    events: list[dict] = []
    for change in item["changes"]:
      kind = change["kind"]
      if kind not in {"add", "update"}:
        continue
      path = change["path"]
      events.append({"type": ET.FILE_WRITE, "path": path})
    return events

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
