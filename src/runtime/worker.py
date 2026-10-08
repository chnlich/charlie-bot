"""Worker Agent — spawns and monitors Claude Code CLI subprocesses."""

import asyncio
import json
import os
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import orjson

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.deferred import deferred_module_getattr
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import BackendOption, SessionMetadata, ThreadMetadata, utc_now_iso
from src.infra.ndjson import append_ndjson
from src.infra.ndjson import write_all as _write_all
from src.infra.process import kill_group_escalating
from src.runtime import launch_loop, runs
from src.runtime.agent_process.base import (
    AgentBackend,
    _capture_proc_diagnostics,
    _read_stderr_tail,
    tail_follow_events,
)
from src.runtime.agent_process.deferred_build import load_build_backend
from src.runtime.hooks import backend_lifecycle, backend_types
from src.runtime.session_usage import _prompt_token_sum
from src.runtime.streaming import handle_compaction_events, streaming_manager

log = LazyStructlogLogger()

# Directory holding the tracked `git` wrapper every worker/reviewer child runs
# with first on PATH (src/runtime/git_stash_guard/git). This file sits at
# src/runtime/, so the guard is the sibling git_stash_guard/ directory - derived
# from the module location, never a host path literal.
GIT_STASH_GUARD_DIR = Path(__file__).resolve().parent / "git_stash_guard"


def __getattr__(name: str) -> Any:
  # The "src.runtime.worker.build_backend" patch target resolves through this hook.
  return deferred_module_getattr(name, __name__, globals(), "build_backend", load_build_backend)


# Each entry is a substring catch-all for its family: bare "quota" also matches
# every phrase form ("quota exceeded", ...), so phrase entries stay out.
QUOTA_ERROR_PATTERNS = [
    "rate limit",
    "resource_exhausted",
    "429",
    "quota",
]


class QuotaExhaustedError(Exception):
  pass


def _clamp_ts(clamp_to: datetime | None) -> str:
  """Timestamp for synthesized (non-raw) events: now, capped at the run's end.

  All events of a run must satisfy timestamp <= completed_at, and completed_at
  is the raw log's final mtime; capping synthesized events at that mtime makes
  the invariant hold even when finalization runs long after the run ended.
  """
  now = datetime.now(UTC)
  if clamp_to is not None and clamp_to < now:
    return clamp_to.isoformat()
  return now.isoformat()


def _event_line(event: dict) -> bytes:
  """Serialize one persisted worker event to its log line, as wire bytes."""
  # orjson because the per-event serialization rides the streamed-turn head
  # (the collector's worst-single-event reading); every reader JSON-parses the
  # log per line, so the compact UTF-8 byte form is inert. Every persisted
  # event is a machine-built dict of JSON-parsed values — str keys only.
  # The line stays bytes end to end: a decode-to-str hop costs a full
  # decode + str concat + re-encode of the payload per event (64 ms of the
  # 76 ms head on a 9.5 MB tool_result), and orjson's output is already the
  # UTF-8 bytes the log carries.
  return orjson.dumps(event) + b"\n"


async def _append_event_line(fd: int, line: bytes) -> None:
  # On-loop write: the events log is page-cached and append-only, so os.write
  # costs single-digit microseconds on a typical event and its worst case is
  # the write itself (~90 us per 100 KB). The executor hop bought no
  # durability — the fd carries no fdatasync; the events log is a diagnostic
  # stream, not the fdatasync-durable chat funnel (append_ndjson) — and cost a
  # scheduler round-trip per event whose wakeup under load can spike to
  # milliseconds, the streamed-turn head this append rides.
  _write_all(fd, line)


class Worker:
  """Manages the backend subprocesses of one task.

  One task is one process, except for a backend whose lifecycle relays (``backend_lifecycle``):
  when the watch of a process asks for a next one, the lifecycle plans it and the Worker runs it
  (``launch_loop``). Every process of the task appends to the same events log; the terminal
  events belong to the last one. ``session_meta`` is the session the task belongs to; a run
  needs it, and a follow of an interrupted run does not.
  """

  def __init__(
      self,
      thread_metadata: ThreadMetadata,
      working_dir: Path,
      events_log_path: Path,
      task_description: str,
      cfg: CharlieBotConfig,
      backend_option: BackendOption | None = None,
      extra_env: dict[str, str] | None = None,
      on_spawned: Callable | None = None,
      instructions_content: str | None = None,
      session_meta: SessionMetadata | None = None,
  ) -> None:
    self._thread = thread_metadata
    self._worktree = working_dir
    self._events_log = events_log_path
    self._task_description = task_description
    self._cfg = cfg
    self._backend_option = backend_option
    self._extra_env = extra_env or {}
    self._on_spawned = on_spawned
    self._instructions_content = instructions_content
    self._session_meta = session_meta
    self._backend: AgentBackend | None = None
    # The plan and the watch of the process now running; None outside a launched run.
    self._launch: backend_lifecycle.Launch | None = None
    self._watch: backend_lifecycle.LaunchWatch | None = None
    self._relays = 0
    # Context size from the newest assistant usage block, for the relay compaction rule.
    self._context_tokens: int | None = None
    # Session-level notices (the login-required event) leave through this hook; the
    # run entry that knows the session binds it (task_execution's event streaming).
    self.on_session_event: Callable[[dict], Awaitable[None]] | None = None

  @property
  def account_relays(self) -> int:
    return self._relays

  def _build_backend(
      self,
      on_spawn: Callable[[int], Awaitable[None]] | None,
      launch: backend_lifecycle.Launch | None = None,
  ) -> AgentBackend:
    """Build the backend for this task; *on_spawn* is None for translate-only instances.

    *launch* is the lifecycle's plan for the process; a translate-only build has none.
    Launcher builds (on_spawn set) fail loudly: a missing CLI binary raises in
    the constructor and a real run never silently degrades to another backend.
    Translate-only builds (on_spawn None) only parse events, so a construction
    failure (e.g. the host lacks the backend's CLI binary) degrades to the
    method's binary-free fallback branch — same shape as spawner's stale-id
    translate-only fallback — and never crashes restart recovery's drain.
    """
    if self._backend_option:
      launch_kwargs = dict(launch.backend_kwargs) if launch is not None else {}
      extra_flags = launch_kwargs.pop("extra_flags", [])
      backend_kwargs: dict[str, Any] = {
          "buffer_limit": self._cfg.subprocess_buffer_limit,
          "on_spawn": on_spawn,
          "instructions_content": self._instructions_content,
          "log_dir": self._events_log.parent,
          "cgroup_session_id": self._thread.session_id,
          **launch_kwargs,
      }
      if backend_types.traits_for(self._backend_option.type).preassigned_session_id:
        # The runtime chose this task's session id before the first process: a relay resumes it
        # with --resume, and the first process opens it.
        if launch is not None and launch.resume_id:
          extra_flags = ["--resume", launch.resume_id, *extra_flags]
        else:
          backend_kwargs["claude_session_id"] = self._thread.claude_session_id
      if extra_flags:
        backend_kwargs["extra_flags"] = extra_flags
      try:
        backend = load_build_backend(globals())
        return backend(self._backend_option, self._cfg, **backend_kwargs)
      except Exception as e:
        if on_spawn is not None:
          raise
        log.warning(
            "translate_backend_unresolved",
            thread_id=self._thread.id,
            backend=self._backend_option.id,
            backend_type=self._backend_option.type,
            error=str(e))
    # Fallback to the binary-free translate backend
    return backend_types.build_translate_fallback(
        self._cfg,
        buffer_limit=self._cfg.subprocess_buffer_limit,
        on_spawn=on_spawn,
        instructions_content=self._instructions_content,
        log_dir=self._events_log.parent,
        cgroup_session_id=self._thread.session_id,
    )

  def _launch_context(self, fd: int) -> backend_lifecycle.LaunchContext:
    """The ``LaunchContext`` of this task run: events go to the events log and the session's chat."""
    assert self._backend_option is not None and self._session_meta is not None

    async def emit(event: dict) -> None:
      await self._persist_and_broadcast(fd, event)
      if self.on_session_event is not None:
        await self.on_session_event(event)

    async def record_account(label: str) -> None:
      """A task keeps no account label: the label is a master turn's session field."""

    async def context_state() -> tuple[int | None, datetime | None]:
      return self._context_tokens, None

    return backend_lifecycle.LaunchContext(
        cfg=self._cfg,
        option=self._backend_option,
        session_meta=self._session_meta,
        kind="task",
        cwd=str(self._worktree),
        held_native_id=None,
        emit=emit,
        record_account=record_account,
        context_state=context_state)

  async def run(self) -> int:
    """Spawn the Worker and stream its output. Returns exit code."""
    # The supervisor strip runs on the inherited environment; the launch's own
    # extra_env (the child's session id, its signed run token, the selected
    # home) applies AFTER it, so the child's explicit identity survives the
    # inherited-identity strip.
    env = {**launch_loop.child_env(os.environ), **self._extra_env}
    # The stash-guard git wrapper rides first on the child's PATH (plan 3 v3):
    # every worktree of a repository shares one stash stack, so the guard
    # refuses the git stash write forms an agent runs by habit and passes
    # everything else to real git. GIT_STASH_GUARD_DIR holds only that
    # wrapper, derived from this module's location.
    old_path = env.get("PATH")
    env["PATH"] = f"{GIT_STASH_GUARD_DIR}:{old_path}" if old_path else str(GIT_STASH_GUARD_DIR)

    async def _on_spawn(pid: int) -> None:
      self._thread.pid = pid
      # pid_start was pinned to this exact process instance just before this
      # callback fired; persist both atomically so a pid reused after a crash
      # can never fake this run's liveness.
      self._thread.pid_start = self._backend.pid_start
      log.info("worker_spawned", thread=self._thread.id, pid=pid)
      if self._on_spawned:
        await self._on_spawned(self._thread)

    # Read stdout (NDJSON) line by line via the backend; a task whose lifecycle relays loops once
    # per relay, each process appending to the same events log.
    self._events_log.parent.mkdir(parents=True, exist_ok=True)
    # The fd is held for the whole run: a worker's events log is append-only and
    # nothing rewrites or replaces it mid-run (only chat files archive), so a
    # per-call open to re-resolve the path is never needed here.
    fd = os.open(self._events_log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
    exit_code = 1

    async def _run_process(
        launch: backend_lifecycle.Launch,
        watch: backend_lifecycle.LaunchWatch | None,
        relays_before: int,
    ) -> tuple[int, str]:
      nonlocal exit_code
      self._relays = relays_before
      self._launch = launch
      self._watch = watch
      self._backend = self._build_backend(_on_spawn, launch)
      log.info(
          "worker_starting",
          thread=self._thread.id,
          cwd=str(self._worktree),
          account=launch.account_label,
          relays=self._relays)
      task_description = launch.prompt if launch.prompt is not None else self._task_description
      async for event in self._backend.run(task_description, str(self._worktree), env):
        await self._process_event(event, fd)
      exit_code = self._backend.exit_code
      return exit_code, self._backend.stderr_text

    def _on_relay(relays: int) -> None:
      self._relays = relays

    try:
      if self._backend_option is None:
        # No backend option: one process on the binary-free fallback backend.
        await _run_process(backend_lifecycle.Launch(backend_kwargs={}, resume_id=None), None, 0)
      else:
        ctx = self._launch_context(fd)
        lifecycle = backend_types.lifecycle_for(self._backend_option)
        # A refused launch raises LaunchRefused to the run entry, which ends the run with its message.
        await launch_loop.run_launches(
            ctx,
            lifecycle,
            run_process=_run_process,
            native_id=lambda: self._thread.claude_session_id,
            on_relay=_on_relay,
        )
    finally:
      os.close(fd)

    completion = runs.raw_completion_time(self._raw_log_path())
    await self._emit_terminal_events(
        exit_code,
        self._backend.stderr_text,
        self._backend.hang_diagnostics,
        cgroup_report=self._backend.cgroup_exit_report(),
        clamp_to=completion,
    )
    log.info("worker_finished", thread=self._thread.id, exit_code=exit_code)
    return exit_code

  async def resume(
      self,
      *,
      is_alive: Callable[[], bool],
      on_silence: Callable[[], Awaitable[None]] | None,
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
    fd = os.open(self._events_log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o666)
    try:
      async for event in tail_follow_events(
          raw_path,
          translate=stream_backend.translate_event,
          is_alive=is_alive,
          cursor=cursor,
          start_offset=runs.read_raw_cursor(cursor),
          post_result_timeout=stream_backend._POST_RESULT_TIMEOUT,
          buffer_limit=self._cfg.subprocess_buffer_limit,
          on_silence=on_silence,
      ):
        await self._process_event(event, fd)
    finally:
      os.close(fd)

    _, _, exit_code = runs.scan_result_exit(raw_path, self._build_backend(None).translate_event)

    # The loop ended on the post-result timeout while the process is still
    # alive: same contract as the live path's cleanup — capture diagnostics,
    # then kill the process group (never reached on a stalled-no-result run;
    # that one keeps following until the process truly exits).
    hang_diagnostics = None
    if is_alive():
      hang_diagnostics = await _capture_proc_diagnostics(self._thread.pid)
      if self._thread.pid is not None:
        await kill_group_escalating(self._thread.pid, is_alive)

    completion = runs.raw_completion_time(raw_path)
    await self._emit_terminal_events(
        exit_code,
        await asyncio.to_thread(_read_stderr_tail, stderr_path),
        hang_diagnostics,
        clamp_to=completion,
    )
    log.info("worker_resume_finished", thread=self._thread.id, exit_code=exit_code)
    return exit_code

  async def _persist_and_broadcast(self, fd: int, event: dict) -> None:
    if not event.get("timestamp"):
      event["timestamp"] = utc_now_iso()
    await _append_event_line(fd, _event_line(event))
    await streaming_manager.broadcast(self._thread.id, event)

  def _raw_log_path(self) -> Path:
    # The events log lives in <thread>/data/, which is also the backend's
    # log_dir, so the raw name joins onto the dir the worker already holds.
    return self._events_log.parent / runs.RAW_LOG_NAME

  async def _emit_terminal_events(
      self,
      exit_code: int,
      stderr_text: str,
      hang_diagnostics: dict | None,
      *,
      cgroup_report: str | None = None,
      clamp_to: datetime | None,
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

    if cgroup_report:
      # Session memory-cap / host-OOM attribution: the
      # worker failure message channel, so the report reaches the session chat
      # through the same events log the finalize path reads.
      cap_event = {"type": ET.ERROR, "content": cgroup_report, "timestamp": _clamp_ts(clamp_to)}
      await append_ndjson(self._events_log, cap_event)
      await streaming_manager.broadcast(self._thread.id, cap_event)
      log.warning("worker_cgroup_exit_report", thread=self._thread.id, report=cgroup_report)

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

  async def _process_event(self, event_data: dict, fd: int) -> None:
    """Write event to disk log and broadcast to WebSocket subscribers."""
    # The run-start adoption signal is the worker log's session-id record (the
    # token tally's codex reconciliation reads the id from the raw line), but
    # it carries no renderable content: the projection skips it and no thread
    # subscriber reads it, so the broadcast frame is pure waste.
    if event_data.get("type") == ET.SESSION_ATTACHED:
      if not event_data.get("timestamp"):
        event_data["timestamp"] = utc_now_iso()
      await _append_event_line(fd, _event_line(event_data))
      return
    # Detect quota exhaustion errors. The payload copies ride only the type the
    # pattern check reads: str() reprs the whole message dict and lower() copies
    # it per streamed event, while QUOTA_ERROR_PATTERNS can only match on ERROR.
    event_type = event_data.get("type", "")
    event_message = event_content = ""
    if event_type == ET.ERROR:
      event_message = str(event_data.get("message", "")).lower()
      event_content = str(event_data.get("content", "")).lower()

    # Ensure all persisted events carry a stable event-time.
    if not event_data.get("timestamp"):
      event_data["timestamp"] = utc_now_iso()

    # A task whose lifecycle relays folds every event into its watch; a rejection ends
    # the process on its own and the relay follows in run(), so the event is
    # persisted like any other instead of raising.
    terminate_now = self._watch is not None and self._watch.observe(event_data)

    # Detect rate-limit rejections from Claude Code (type=ET.RATE_LIMIT_EVENT)
    if event_type == ET.RATE_LIMIT_EVENT:
      rli = event_data.get(ET.RATE_LIMIT_INFO, {})
      if rli.get("status") == "rejected":
        rate_type = rli.get("rateLimitType", "unknown")
        resets_at = rli.get("resetsAt", "unknown")
        log.warning(
            "worker_rate_limited",
            thread=self._thread.id,
            rate_type=rate_type,
            resets_at=resets_at,
            account=self._launch.account_label if self._launch is not None else None)
        if self._watch is None:
          await _append_event_line(fd, _event_line(event_data))
          raise QuotaExhaustedError(f"Rate limited ({rate_type}), resets at {resets_at}")

    if event_type == ET.ASSISTANT:
      message = event_data.get("message")
      usage = message.get("usage") if isinstance(message, dict) else None
      if isinstance(usage, dict) and _prompt_token_sum(usage) > 0:
        self._context_tokens = _prompt_token_sum(usage)

    if event_type == ET.ERROR and any(p in event_message or p in event_content for p in QUOTA_ERROR_PATTERNS):
      await _append_event_line(fd, _event_line(event_data))
      raise QuotaExhaustedError(event_data.get("message", "Quota exhausted"))

    # Write to disk
    await _append_event_line(fd, _event_line(event_data))

    # Broadcast to WebSocket subscribers
    await streaming_manager.broadcast(self._thread.id, event_data)

    async def _persist_and_broadcast(evt: dict) -> None:
      await _append_event_line(fd, _event_line(evt))
      await streaming_manager.broadcast(self._thread.id, evt)

    await handle_compaction_events(
        event_data,
        persist_and_broadcast=_persist_and_broadcast,
        log_context={"thread": self._thread.id},
    )

    if terminate_now:
      # Armed relay at its safe point: the tool result is on disk, stop here.
      assert self._backend is not None and self._launch is not None
      log.warning("worker_account_relay_safe_point", thread=self._thread.id, account=self._launch.account_label)
      await self._backend.terminate()
