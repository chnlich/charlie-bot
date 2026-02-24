"""Worker Agent — spawns and monitors Claude Code CLI subprocesses."""

import json
import os
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.agents.backends.claude_code import BASE_COMMAND as WORKER_COMMAND, ClaudeCodeBackend
from src.core.config import get_config
from src.core.models import ThreadMetadata
from src.core.ndjson import append_ndjson
from src.core.streaming import streaming_manager

log = structlog.get_logger()

QUOTA_ERROR_PATTERNS = [
    "quota exceeded",
    "rate limit",
    "resource_exhausted",
    "429",
    "quota",
]


class QuotaExhaustedException(Exception):
  pass


class Worker:
  """Manages a single Claude Code Worker subprocess for one task."""

  def __init__(
      self,
      thread_metadata: ThreadMetadata,
      working_dir: Path,
      events_log_path: Path,
      task_description: str,
      extra_env: Optional[dict[str, str]] = None,
      on_spawned: Optional[callable] = None,
  ):
    self._thread = thread_metadata
    self._worktree = working_dir
    self._events_log = events_log_path
    self._task_description = task_description
    self._extra_env = extra_env or {}
    self._on_spawned = on_spawned
    self._backend: Optional[ClaudeCodeBackend] = None

  async def run(self) -> int:
    """Spawn the Worker and stream its output. Returns exit code."""
    env = {**os.environ, **self._extra_env}
    env.pop("CLAUDECODE", None)  # Allow worker to spawn Claude Code subprocess

    async def _on_spawn(pid: int) -> None:
      self._thread.pid = pid
      log.info("worker_spawned", thread=self._thread.id, pid=pid)
      if self._on_spawned:
        await self._on_spawned(self._thread)

    self._backend = ClaudeCodeBackend(
        buffer_limit=get_config().subprocess_buffer_limit,
        on_spawn=_on_spawn,
    )

    log.info("worker_starting", thread=self._thread.id, cwd=str(self._worktree))

    # Read stdout (NDJSON) line by line via the backend
    self._events_log.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(self._events_log, "a", encoding="utf-8") as log_file:
      async for event in self._backend.run(self._task_description, str(self._worktree), env):
        await self._process_event(event, log_file)

    exit_code = self._backend.exit_code

    if self._backend.stderr_text:
      stderr_event = {"type": "error", "content": self._backend.stderr_text}
      await append_ndjson(self._events_log, stderr_event)
      await streaming_manager.broadcast(self._thread.id, stderr_event)
      log.warning("worker_stderr", thread=self._thread.id, stderr=self._backend.stderr_text[:500])

    # Emit final completion event
    final_event = {
        "type": "complete" if exit_code == 0 else "error",
        "status": "success" if exit_code == 0 else "failed",
        "exit_code": exit_code,
    }
    await streaming_manager.broadcast(self._thread.id, final_event)
    log.info("worker_finished", thread=self._thread.id, exit_code=exit_code)
    return exit_code

  async def terminate(self) -> None:
    """Terminate the Worker subprocess if still running."""
    if self._backend is not None:
      await self._backend.terminate()

  async def _process_event(self, event_data: dict, log_file) -> None:
    """Write event to disk log and broadcast to WebSocket subscribers."""
    # Detect quota exhaustion errors
    event_type = event_data.get("type", "")
    event_message = str(event_data.get("message", "")).lower()
    event_content = str(event_data.get("content", "")).lower()

    if event_type == "error" and any(p in event_message or p in event_content for p in QUOTA_ERROR_PATTERNS):
      await log_file.write(json.dumps(event_data) + "\n")
      await log_file.flush()
      raise QuotaExhaustedException(event_data.get("message", "Quota exhausted"))

    # Write to disk
    await log_file.write(json.dumps(event_data) + "\n")
    await log_file.flush()

    # Broadcast to WebSocket subscribers
    await streaming_manager.broadcast(self._thread.id, event_data)

    if event_data.get("type") == "system" and event_data.get("subtype") == "compact_boundary":
      meta = event_data.get("compact_metadata", {})
      trigger = meta.get("trigger", "unknown")
      pre_tokens = meta.get("pre_tokens")
      log.info("cc_context_compacted", thread=self._thread.id, trigger=trigger, pre_tokens=pre_tokens)
      compact_event = {
          "type": "context_compacted",
          "trigger": trigger,
          "pre_tokens": pre_tokens,
      }
      await log_file.write(json.dumps(compact_event) + "\n")
      await log_file.flush()
      await streaming_manager.broadcast(self._thread.id, compact_event)
