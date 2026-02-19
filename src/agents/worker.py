"""Worker Agent — spawns and monitors Claude Code CLI subprocesses."""

import asyncio
import json
import os
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.core.models import ThreadMetadata, WorkerEvent
from src.core.streaming import streaming_manager

log = structlog.get_logger()

WORKER_COMMAND = [
  "claude",
  "-p",
  "--dangerously-skip-permissions",
  "--output-format",
  "stream-json",
  "--verbose",
]

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
    worktree_path: Path,
    events_log_path: Path,
    task_description: str,
    extra_env: Optional[dict[str, str]] = None,
    on_spawned: Optional[callable] = None,
  ):
    self._thread = thread_metadata
    self._worktree = worktree_path
    self._events_log = events_log_path
    self._task_description = task_description
    self._extra_env = extra_env or {}
    self._on_spawned = on_spawned
    self._proc: Optional[asyncio.subprocess.Process] = None

  async def run(self) -> int:
    """Spawn the Worker and stream its output. Returns exit code."""
    env = {**os.environ, **self._extra_env}
    env.pop("CLAUDECODE", None)  # Allow worker to spawn Claude Code subprocess

    # Claude reads CLAUDE.md from cwd automatically; pass task via stdin prompt
    cmd = WORKER_COMMAND + [self._task_description]

    log.info(
      "worker_starting",
      thread=self._thread.id,
      cwd=str(self._worktree),
    )

    self._proc = await asyncio.create_subprocess_exec(
      *cmd,
      cwd=str(self._worktree),
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
      env=env,
    )

    self._thread.pid = self._proc.pid
    log.info("worker_spawned", thread=self._thread.id, pid=self._proc.pid)

    # Persist PID to disk so startup recovery can check process liveness
    if self._on_spawned:
      await self._on_spawned(self._thread)

    # Read stdout (NDJSON) line by line
    self._events_log.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(self._events_log, "a", encoding="utf-8") as log_file:
      assert self._proc.stdout is not None
      async for raw_line in self._proc.stdout:
        line = raw_line.decode("utf-8", errors="replace").strip()
        if not line:
          continue
        await self._process_line(line, log_file)

    # Capture any stderr output (crash messages, missing command, etc.)
    assert self._proc.stderr is not None
    stderr_bytes = await self._proc.stderr.read()

    await self._proc.wait()
    exit_code = self._proc.returncode or 0

    if stderr_bytes:
      stderr_text = stderr_bytes.decode("utf-8", errors="replace").strip()
      if stderr_text:
        stderr_event = json.dumps({"type": "error", "content": stderr_text})
        async with aiofiles.open(self._events_log, "a", encoding="utf-8") as f:
          await f.write(stderr_event + "\n")
        await streaming_manager.broadcast(self._thread.id, {"type": "error", "content": stderr_text})
        log.warning("worker_stderr", thread=self._thread.id, stderr=stderr_text[:500])

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
    if self._proc and self._proc.returncode is None:
      self._proc.terminate()
      try:
        await asyncio.wait_for(self._proc.wait(), timeout=5.0)
      except asyncio.TimeoutError:
        log.warning("worker_terminate_timeout", thread=self._thread.id, pid=self._proc.pid)
        self._proc.kill()

  async def _process_line(self, line: str, log_file) -> None:
    """Parse a NDJSON line, write to disk log, and broadcast to WebSocket subscribers."""
    try:
      event_data = json.loads(line)
    except json.JSONDecodeError as e:
      log.debug("worker_line_not_json", thread=self._thread.id, error=str(e))
      event_data = {"type": "raw", "content": line}

    # Detect quota exhaustion errors
    event_type = event_data.get("type", "")
    event_message = str(event_data.get("message", "")).lower()
    event_content = str(event_data.get("content", "")).lower()

    if event_type == "error" and any(p in event_message or p in event_content for p in QUOTA_ERROR_PATTERNS):
      await log_file.write(line + "\n")
      await log_file.flush()
      raise QuotaExhaustedException(event_data.get("message", "Quota exhausted"))

    # Write to disk
    await log_file.write(line + "\n")
    await log_file.flush()

    # Broadcast to WebSocket subscribers
    await streaming_manager.broadcast(self._thread.id, event_data)
