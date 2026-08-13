"""Worker Agent — spawns and monitors Claude Code CLI subprocesses."""

import asyncio
import json
import os
import signal
import time
from collections.abc import Awaitable, Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import aiofiles
import structlog

from src.agents.backends.base import (
    AgentBackend,
    _capture_proc_diagnostics,
    _read_stderr_tail,
    tail_follow_events,
)
from src.agents.backends.claude_code import ClaudeCodeBackend
from src.agents.backends.registry import build_backend
from src.core import event_types as ET
from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, ThreadMetadata
from src.core.ndjson import append_ndjson
from src.core.process import kill_process_group
from src.core.streaming import handle_compaction_events, streaming_manager

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


def _clamp_ts(clamp_to: Optional[datetime]) -> str:
  """Timestamp for synthesized (non-raw) events: now, capped at the run's end.

  All events of a run must satisfy timestamp <= completed_at, and completed_at
  is the raw log's final mtime; capping synthesized events at that mtime makes
  the invariant hold even when finalization runs long after the run ended.
  """
  now = datetime.now(timezone.utc)
  if clamp_to is not None and clamp_to < now:
    return clamp_to.isoformat()
  return now.isoformat()


class Worker:
  """Manages a single Claude Code Worker subprocess for one task."""

  def __init__(
      self,
      thread_metadata: ThreadMetadata,
      working_dir: Path,
      events_log_path: Path,
      task_description: str,
      cfg: CharlieBotConfig,
      backend_option: Optional[BackendOption] = None,
      extra_env: Optional[dict[str, str]] = None,
      on_spawned: Optional[callable] = None,
      instructions_content: Optional[str] = None,
  ):
    self._thread = thread_metadata
    self._worktree = working_dir
    self._events_log = events_log_path
    self._task_description = task_description
    self._cfg = cfg
    self._backend_option = backend_option
    self._extra_env = extra_env or {}
    self._on_spawned = on_spawned
    self._instructions_content = instructions_content
    self._backend: Optional[AgentBackend] = None

  def _build_backend(self, on_spawn: Optional[Callable[[int], Awaitable[None]]]) -> AgentBackend:
    """Build the backend for this task; *on_spawn* is None for translate-only instances.

    Launcher builds (on_spawn set) fail loudly: a missing CLI binary raises in
    the constructor and a real run never silently degrades to another backend.
    Translate-only builds (on_spawn None) only parse events, so a construction
    failure (e.g. the host lacks the backend's CLI binary) degrades to the
    method's binary-free fallback branch — same shape as spawner's stale-id
    translate-only fallback — and never crashes restart recovery's drain.
    """
    if self._backend_option:
      backend_kwargs = {
          "buffer_limit": self._cfg.subprocess_buffer_limit,
          "on_spawn": on_spawn,
          "instructions_content": self._instructions_content,
          "log_dir": self._events_log.parent,
      }
      if self._backend_option.type == "cc-claude":
        backend_kwargs["claude_session_id"] = self._thread.claude_session_id
      if on_spawn is not None:
        return build_backend(self._backend_option, self._cfg, **backend_kwargs)
      try:
        return build_backend(self._backend_option, self._cfg, **backend_kwargs)
      except Exception as e:
        log.warning(
            "translate_backend_unresolved",
            thread_id=self._thread.id,
            backend=self._backend_option.id,
            backend_type=self._backend_option.type,
            error=str(e))
    # Fallback to default ClaudeCodeBackend
    return ClaudeCodeBackend(
        buffer_limit=self._cfg.subprocess_buffer_limit,
        on_spawn=on_spawn,
        instructions_content=self._instructions_content,
        log_dir=self._events_log.parent,
    )

  async def run(self) -> int:
    """Spawn the Worker and stream its output. Returns exit code."""
    env = {**os.environ, **self._extra_env}
    env.pop("CLAUDECODE", None)  # Allow worker to spawn Claude Code subprocess
    env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"

    async def _on_spawn(pid: int) -> None:
      self._thread.pid = pid
      # pid_start was pinned to this exact process instance just before this
      # callback fired; persist both atomically so a pid reused after a crash
      # can never fake this run's liveness.
      self._thread.pid_start = self._backend.pid_start
      log.info("worker_spawned", thread=self._thread.id, pid=pid)
      if self._on_spawned:
        await self._on_spawned(self._thread)

    self._backend = self._build_backend(_on_spawn)

    log.info("worker_starting", thread=self._thread.id, cwd=str(self._worktree))

    # Read stdout (NDJSON) line by line via the backend
    self._events_log.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(self._events_log, "a", encoding="utf-8") as log_file:
      async for event in self._backend.run(self._task_description, str(self._worktree), env):
        await self._process_event(event, log_file)

    exit_code = self._backend.exit_code
    completion = runs.raw_completion_time(self._raw_log_path())
    await self._emit_terminal_events(
        exit_code,
        self._backend.stderr_text,
        self._backend.hang_diagnostics,
        clamp_to=completion,
    )
    log.info("worker_finished", thread=self._thread.id, exit_code=exit_code)
    return exit_code

  async def resume(
      self,
      *,
      is_alive: Callable[[], bool],
      on_silence: Optional[Callable[[], Awaitable[None]]] = None,
  ) -> int:
    """Re-attach to an interrupted run and stream its remaining output.

    Consumer-side this is identical to run(): the same tail-follow loop (from
    the persisted cursor), the same per-event processing, the same terminal
    events. Only the truth source differs — liveness comes from the caller's
    (pid, pid_start) judgment instead of an in-process handle, and the exit
    code is derived from the raw log's trailing result event (the run's true
    outcome, independent of the exit code a long-gone process had).

    ``on_silence`` is the follow-time silence recheck, forwarded to the
    tail-follow loop; it reports, never judges.
    """
    data_dir = self._events_log.parent
    raw_path = self._raw_log_path()
    stderr_path = data_dir / runs.STDERR_LOG_NAME
    cursor = data_dir / runs.CURSOR_NAME

    # A dedicated translate instance for the stream; translate_event is
    # stateful on some backends and one instance must own the stream.
    stream_backend = self._build_backend(None)

    self._events_log.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(self._events_log, "a", encoding="utf-8") as log_file:
      async for event in tail_follow_events(
          raw_path,
          translate=stream_backend.translate_event,
          is_alive=is_alive,
          cursor=cursor,
          start_offset=runs.read_raw_cursor(cursor),
          post_result_timeout=stream_backend._POST_RESULT_TIMEOUT,
          on_silence=on_silence,
      ):
        await self._process_event(event, log_file)

    # Whole-file result scan with a FRESH translate instance (stateful
    # translates may not reuse one that already consumed a stream tail). A
    # missing raw log is a legal drain input (never-started respawn fallback,
    # legacy thread without the new transport): it scans to no result.
    fresh_backend = self._build_backend(None)
    if raw_path.is_file():
      events = runs.project_raw_events(runs.parse_raw_lines(raw_path.read_bytes()), fresh_backend.translate_event)
    else:
      events = []
    result = runs.summarize_result(events)
    exit_code = 0 if (result is not None and runs.result_success(result)) else -1

    # The loop ended on the post-result timeout while the process is still
    # alive: same contract as the live path's cleanup — capture diagnostics,
    # then kill the process group (never reached on a stalled-no-result run;
    # that one keeps following until the process truly exits).
    hang_diagnostics = None
    if is_alive():
      hang_diagnostics = await _capture_proc_diagnostics(self._thread.pid)
      await self._kill_detached_run(is_alive)

    completion = runs.raw_completion_time(raw_path)
    await self._emit_terminal_events(
        exit_code,
        await asyncio.to_thread(_read_stderr_tail, stderr_path),
        hang_diagnostics,
        clamp_to=completion,
    )
    log.info("worker_resume_finished", thread=self._thread.id, exit_code=exit_code)
    return exit_code

  async def _kill_detached_run(self, is_alive: Callable[[], bool]) -> None:
    """SIGTERM a detached (re-attached) run's process group; escalate to SIGKILL."""
    if self._thread.pid is None:
      return
    kill_process_group(self._thread.pid, signal.SIGTERM)
    deadline = time.monotonic() + 5.0
    while is_alive() and time.monotonic() < deadline:
      await asyncio.sleep(0.2)
    if is_alive():
      kill_process_group(self._thread.pid, signal.SIGKILL)

  def _raw_log_path(self) -> Path:
    # The events log lives in <thread>/data/, which is also the backend's
    # log_dir — runs.*_path takes the THREAD dir, so join names directly here.
    return self._events_log.parent / runs.RAW_LOG_NAME

  async def _emit_terminal_events(
      self,
      exit_code: int,
      stderr_text: str,
      hang_diagnostics: Optional[dict],
      *,
      clamp_to: Optional[datetime],
  ) -> None:
    """Persist/broadcast the synthesized post-run events shared by run() and resume()."""
    if hang_diagnostics:
      diag_path = self._events_log.parent / "hang_diagnostics.json"
      try:
        await asyncio.to_thread(diag_path.write_text, json.dumps(hang_diagnostics, indent=2))
        log.warning("worker_wrote_hang_diagnostics", thread=self._thread.id, path=str(diag_path))
      except Exception as e:
        log.error("worker_write_hang_diagnostics_failed", thread=self._thread.id, error=str(e))
      diag_event = {
          "type": ET.SYSTEM,
          "subtype": "hang_diagnostics",
          "diagnostics_path": str(diag_path),
          "exit_code": exit_code,
          "timestamp": _clamp_ts(clamp_to),
      }
      await append_ndjson(self._events_log, diag_event)
      await streaming_manager.broadcast(self._thread.id, diag_event)

    if stderr_text:
      stderr_event_type = ET.ERROR if exit_code != 0 else ET.SYSTEM
      stderr_event = {
          "type": stderr_event_type,
          "content": stderr_text,
          "timestamp": _clamp_ts(clamp_to),
      }
      if stderr_event_type == ET.SYSTEM:
        stderr_event["subtype"] = "stderr"
      await append_ndjson(self._events_log, stderr_event)
      await streaming_manager.broadcast(self._thread.id, stderr_event)
      log.warning("worker_stderr", thread=self._thread.id, stderr=stderr_text[:500])

    # Emit final completion event
    final_event = {
        "type": ET.COMPLETE if exit_code == 0 else ET.ERROR,
        "status": "success" if exit_code == 0 else "failed",
        "exit_code": exit_code,
        "timestamp": _clamp_ts(clamp_to),
    }
    await streaming_manager.broadcast(self._thread.id, final_event)

  async def terminate(self) -> None:
    """Terminate the Worker subprocess if still running."""
    if self._backend is not None:
      await self._backend.terminate()

  def detach(self) -> None:
    """Forget the running subprocess without signalling it (shutdown let-go)."""
    if self._backend is not None:
      self._backend.detach()

  async def _process_event(self, event_data: dict, log_file) -> None:
    """Write event to disk log and broadcast to WebSocket subscribers."""
    # Detect quota exhaustion errors
    event_type = event_data.get("type", "")
    event_message = str(event_data.get("message", "")).lower()
    event_content = str(event_data.get("content", "")).lower()

    # Ensure all persisted events carry a stable event-time.
    if not event_data.get("timestamp"):
      event_data["timestamp"] = datetime.now(timezone.utc).isoformat()

    # Detect rate-limit rejections from Claude Code (type=ET.RATE_LIMIT_EVENT)
    if event_type == ET.RATE_LIMIT_EVENT:
      rli = event_data.get("rate_limit_info", {})
      if rli.get("status") == "rejected":
        rate_type = rli.get("rateLimitType", "unknown")
        resets_at = rli.get("resetsAt", "unknown")
        log.warning("worker_rate_limited", thread=self._thread.id, rate_type=rate_type, resets_at=resets_at)
        await log_file.write(json.dumps(event_data) + "\n")
        await log_file.flush()
        raise QuotaExhaustedException(f"Rate limited ({rate_type}), resets at {resets_at}")

    if event_type == ET.ERROR and any(p in event_message or p in event_content for p in QUOTA_ERROR_PATTERNS):
      await log_file.write(json.dumps(event_data) + "\n")
      await log_file.flush()
      raise QuotaExhaustedException(event_data.get("message", "Quota exhausted"))

    # Write to disk
    await log_file.write(json.dumps(event_data) + "\n")
    await log_file.flush()

    # Broadcast to WebSocket subscribers
    await streaming_manager.broadcast(self._thread.id, event_data)

    async def _persist_and_broadcast(evt: dict) -> None:
      await log_file.write(json.dumps(evt) + "\n")
      await log_file.flush()
      await streaming_manager.broadcast(self._thread.id, evt)

    await handle_compaction_events(
        event_data,
        persist_and_broadcast=_persist_and_broadcast,
        log_context={"thread": self._thread.id},
    )
