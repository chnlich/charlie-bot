"""Abstract base class for agent subprocess backends with template-method run().

Restart-safe transport: the subprocess writes stdout straight to
``<log_dir>/agent.raw.ndjson`` and stderr to ``<log_dir>/agent.stderr.log`` via
inherited file descriptors, so the writer never depends on the server process
as a reader. The read side is a tail-follow offset loop
(:func:`tail_follow_events`), which is what lets a restarted server re-attach
at the recorded cursor without the agent noticing anything.
"""

import asyncio
import contextlib
import json
import os
import shutil
import signal
import tempfile
import time
from abc import ABC, abstractmethod
from collections.abc import AsyncIterator, Awaitable, Callable
from datetime import UTC, datetime
from pathlib import Path

import aiofiles
import structlog

from src.core import event_types as ET
from src.core import runs
from src.core.process import kill_process_group
from src.core.timeouts import (
  NO_OUTPUT_REPORT_THRESHOLD,
  SUBPROCESS_DIAG_CAPTURE_TIMEOUT,
)

log = structlog.get_logger()

DEFAULT_BUFFER_LIMIT = 1024 * 1024 * 1024  # 1 GB
_STDERR_TAIL_BYTES = 64 * 1024

# Poll cadence of the tail-follow read loop. Event volume is low (median
# inter-event gap ~54 s measured), so a fixed poll beats an inotify dependency.
_TAIL_POLL_INTERVAL = 0.15


async def _capture_proc_diagnostics(pid: int) -> dict:
  """Best-effort snapshot of a hung subprocess: pgid, ps tree, /proc state, fds, children."""
  out: dict = {"captured_at": datetime.now(UTC).isoformat(), "pid": pid}
  try:
    out["pgid"] = os.getpgid(pid)
  except Exception as e:
    out["pgid"] = f"<error: {e}>"
  pgid_for_cmd = out["pgid"] if isinstance(out["pgid"], int) else pid
  for key, cmd in [
      ("process_tree", ["bash", "-c", f"ps --forest -o pid,pgid,stat,etime,wchan,cmd -g {pgid_for_cmd} 2>&1"]),
      ("fds", ["bash", "-c", f"ls -l /proc/{pid}/fd 2>&1 | head -200"]),
      ("status", ["bash", "-c", f"cat /proc/{pid}/status 2>&1"]),
      ("children", ["bash", "-c", f"ps -o pid,stat,etime,cmd --ppid {pid} 2>&1"]),
  ]:
    try:
      r = await asyncio.create_subprocess_exec(*cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT)
      data, _ = await asyncio.wait_for(r.communicate(), timeout=SUBPROCESS_DIAG_CAPTURE_TIMEOUT)
      out[key] = data.decode("utf-8", errors="replace")
    except Exception as e:
      out[key] = f"<capture failed: {e}>"
  return out


def resolve_binary(name: str, fallback_dir: str) -> str:
  """Resolve a CLI binary by name, falling back to a directory path.

  Checks PATH via shutil.which first, then tries fallback_dir/name.
  Raises FileNotFoundError with a consistent message if neither exists.
  """
  path = shutil.which(name)
  if path:
    return path
  fallback = Path(fallback_dir) / name
  if fallback.exists():
    return str(fallback)
  raise FileNotFoundError(f"{name} binary not found on PATH or at {fallback}")


def prepend_path_dir(env: dict[str, str], dir_path: str) -> None:
  """Prepend dir_path to env's PATH, unless it is already one of the entries.

  The split(":") membership check is exact-entry: a host PATH that already
  lists the dir (the common case for a user bin dir) must not gain a duplicate
  or have the existing entry re-ordered to the front.
  """
  current_path = env.get("PATH", "")
  if dir_path not in current_path.split(":"):
    env["PATH"] = f"{dir_path}:{current_path}"


def make_text_event(text: str) -> dict:
  """Build a CC-compatible assistant-text event."""
  return {"type": ET.ASSISTANT, "message": {"content": [{"type": "text", "text": text}]}}


def make_error_event(msg: str) -> dict:
  """Build a CC-compatible error event."""
  return {"type": ET.ERROR, "message": msg, "content": msg}


def make_result_event(
    input_tokens: int = 0,
    output_tokens: int = 0,
    cache_read: int = 0,
    cache_creation: int = 0,
    cost: float | None = 0,
    context_snapshot: dict | None = None,
) -> dict:
  """Build a CC-compatible result/usage event.

  ``context_snapshot`` is an optional opencode-only key carrying raw per-turn
  tokens and the live model limit (see ``OpenCodeBackend``); when provided it is
  added to the event so the usage resolver's snapshot tier can derive the bar's
  full scale and compaction line. Other backends leave it ``None`` so their
  result events keep their existing shape.
  """
  event = {
      "type": ET.RESULT,
      "result": "",
      "usage":
          {
              "input_tokens": input_tokens,
              "output_tokens": output_tokens,
              "cache_read_input_tokens": cache_read,
              "cache_creation_input_tokens": cache_creation,
          },
      "total_cost_usd": cost,
  }
  if context_snapshot is not None:
    event["context_snapshot"] = context_snapshot
  return event


def make_tool_use_event(name: str, input_data: dict) -> dict:
  """Build a CC-compatible tool_use event (flat format)."""
  return {"type": ET.TOOL_USE, "name": name, "input": input_data}


def make_tool_result_event(tool_name: str, content: str) -> dict:
  """Build a CC-compatible tool_result event keyed by tool_name."""
  return {"type": ET.TOOL_RESULT, "tool_name": tool_name, "content": content}


async def iter_ndjson_events(stdout: asyncio.StreamReader) -> AsyncIterator[dict]:
  """Yield the JSON objects of an NDJSON stream.

  Lines decode as UTF-8 with replacement; blank lines and lines that do not
  parse as JSON are skipped.
  """
  async for raw_line in stdout:
    line = raw_line.decode("utf-8", errors="replace").strip()
    if not line:
      continue
    try:
      yield json.loads(line)
    except json.JSONDecodeError:
      continue


def _clamp_event_timestamp(translated: dict, mtime: float) -> None:
  """Inject an event-time into a translated event that lacks one.

  Approximation is intentional (approved trade-off): the read moment, clamped
  to the raw file's current mtime, so every injected timestamp precedes the
  run's completion time (the raw file's FINAL mtime) no matter how long the
  server was down in between.
  """
  if translated.get("timestamp"):
    return
  now = datetime.now(UTC)
  mtime_dt = datetime.fromtimestamp(mtime, tz=UTC)
  translated["timestamp"] = min(now, mtime_dt).isoformat()


async def tail_follow_events(
    raw_path: Path,
    *,
    translate: Callable[[dict], list[dict]],
    is_alive: Callable[[], bool],
    cursor: Path | None = None,
    start_offset: int = 0,
    post_result_timeout: float,
    poll_interval: float = _TAIL_POLL_INTERVAL,
    on_silence: Callable[[], Awaitable[None]] | None = None,
) -> AsyncIterator[dict]:
  """Tail-follow a raw NDJSON log from *start_offset*, yielding translated events.

  This is the single read loop for both the live path (spawned here) and the
  re-attach path (a restarted server resuming from the persisted cursor).

  Semantics:
    - Reads at a byte offset with a poll cadence; a trailing partial line is
      never processed — it is re-read once completed, or dropped when the
      producer is gone (a torn final write replays as at most a duplicate).
    - After each consumed line whose translated events were yielded (i.e. after
      the caller processed them — a generator resumes post-consumption), the
      *cursor* file advances, so a crash ordering never loses an event.
    - A RESULT event does not end the loop; the producer may keep writing.
      The loop ends when the producer is dead AND drained, or when the raw
      file has not grown for *post_result_timeout* after the first RESULT
      (pending tool calls suppress that clock exactly like the live loop did).
    - ``is_alive()`` is the producer-liveness source: the asyncio process
      handle on the live path, the (pid, pid_start) identity pair after
      re-attach. A producer that closed stdout or had its raw file replaced
      still counts as alive — liveness never reads the fd.
    - ``on_silence`` is the follow-time silence recheck: invoked at most once
      per mount when the raw log's last write ages past
      ``NO_OUTPUT_REPORT_THRESHOLD`` (its silence age is seeded from the
      file's mtime, so pre-follow silence counts). It never judges death —
      the follow continues unchanged afterwards.
  """
  while not raw_path.exists():
    # The raw file is created by the spawner before the subprocess starts, so
    # a missing file here means "never spawned"; once the producer is gone
    # there is nothing to follow.
    if not is_alive():
      return
    await asyncio.sleep(poll_interval)

  offset = start_offset
  saw_result = False
  last_growth = time.monotonic()
  try:
    # Seed the silence clock from the raw log's mtime so output produced
    # before this mount counts toward the recheck threshold.
    last_output_at = time.monotonic() - max(0.0, time.time() - raw_path.stat().st_mtime)
  except OSError:
    last_output_at = last_growth
  silence_reported = False
  pending_tool_calls: set[str] = set()

  with open(raw_path, "rb") as f:
    f.seek(offset)
    buf = b""
    while True:
      chunk = f.read(65536)
      if chunk:
        last_growth = time.monotonic()
        # The producer's last write, anchored to the monotonic clock (the
        # file's mtime — reading pre-mount backlog must not count as output).
        last_output_at = last_growth - max(0.0, time.time() - os.fstat(f.fileno()).st_mtime)
        buf += chunk
        lines = buf.split(b"\n")
        buf = lines.pop()  # trailing partial line; re-read once completed
        for raw_line in lines:
          offset += len(raw_line) + 1
          line = raw_line.decode("utf-8", errors="replace").strip()
          if not line:
            continue
          try:
            event = json.loads(line)
          except json.JSONDecodeError as e:
            log.debug("backend_line_not_json", error=str(e))
            continue
          mtime = os.fstat(f.fileno()).st_mtime
          for translated in translate(event):
            evt_type = translated.get("type")

            # Track pending tool calls — wrapped format (OpenCode/GLM-5):
            # {type: 'assistant', message: {content: [{type: 'tool_use', id: '...'}]}}
            if evt_type == ET.ASSISTANT:
              for item in translated.get("message", {}).get("content", []):
                if isinstance(item, dict) and item.get("type") == ET.TOOL_USE:
                  tool_id = item.get("id", "")
                  if tool_id:
                    pending_tool_calls.add(tool_id)

            # Track pending tool calls — flat format (Codex/Gemini):
            # {type: 'tool_use', id: '...', name: '...'}
            if evt_type == ET.TOOL_USE:
              tool_id = translated.get("id", "")
              if tool_id:
                pending_tool_calls.add(tool_id)

            # Clear pending tool calls on tool_result events — flat format.
            if evt_type == ET.TOOL_RESULT:
              tool_use_id = translated.get("tool_use_id", "")
              if tool_use_id:
                pending_tool_calls.discard(tool_use_id)

            # Clear pending tool calls — wrapped format (Claude Code):
            # {type: 'user', message: {content: [{type: 'tool_result', tool_use_id: '...'}]}}
            if evt_type == ET.USER:
              for item in translated.get("message", {}).get("content", []):
                if isinstance(item, dict) and item.get("type") == ET.TOOL_RESULT:
                  tool_use_id = item.get("tool_use_id", "")
                  if tool_use_id:
                    pending_tool_calls.discard(tool_use_id)

            _clamp_event_timestamp(translated, mtime)
            yield translated

            if not saw_result and evt_type == ET.RESULT:
              if pending_tool_calls:
                log.debug(
                    "backend_result_suppressed_pending_tools",
                    pending=len(pending_tool_calls),
                    tool_ids=list(pending_tool_calls),
                )
              else:
                saw_result = True
                last_growth = time.monotonic()
          if cursor is not None:
            runs.write_raw_cursor(cursor, offset)
        continue

      if saw_result and time.monotonic() - last_growth > post_result_timeout:
        log.warning("backend_post_result_timeout", timeout=post_result_timeout)
        break
      if not is_alive():
        break  # producer gone and fully drained
      if (
          on_silence is not None and not silence_reported
          and time.monotonic() - last_output_at > NO_OUTPUT_REPORT_THRESHOLD
      ):
        silence_reported = True
        await on_silence()
      await asyncio.sleep(poll_interval)

    if buf.strip():
      # Torn final write (producer killed mid-line). Dropping it makes a
      # restart replay the run's tail as at most a duplicate — never a loss.
      log.warning("raw_trailing_torn_line_dropped", bytes=len(buf))


def _rotate_stale_transport(log_dir: Path, raw_path: Path, stderr_path: Path, cursor_path: Path) -> None:
  """Move a previous attempt's transport files aside before a fresh spawn.

  The raw log is opened O_APPEND and a fresh tail-follow always starts at
  offset 0, so a fresh spawn into a log dir that already holds a previous
  attempt's raw bytes (e.g. the VERIFY quota-retry fallback, which respawns
  into the same thread data dir) would replay that entire prior stream —
  including whatever terminal error ended it. Rotated files are kept under a
  numbered suffix distinct from RAW_LOG_NAME/STDERR_LOG_NAME (so resolve_run
  and raw_completion_time keep seeing only the current attempt) for
  debugging, and the stale cursor is removed so it can never point past the
  new file's EOF.
  """
  if not raw_path.exists() or raw_path.stat().st_size == 0:
    return
  suffix = 1
  while (log_dir / f"{raw_path.name}.{suffix}").exists():
    suffix += 1
  raw_path.rename(log_dir / f"{raw_path.name}.{suffix}")
  if stderr_path.exists():
    stderr_path.rename(log_dir / f"{stderr_path.name}.{suffix}")
  cursor_path.unlink(missing_ok=True)


def _read_stderr_tail(stderr_path: Path) -> str:
  """Last 64 KB of the stderr log, decoded; '' when the file is missing."""
  try:
    size = stderr_path.stat().st_size
  except OSError:
    return ""
  with open(stderr_path, "rb") as f:
    if size > _STDERR_TAIL_BYTES:
      f.seek(-_STDERR_TAIL_BYTES, os.SEEK_END)
    data = f.read()
  return data.decode("utf-8", errors="replace").strip()


class AgentBackend(ABC):
  """Abstract interface for running a Claude agent subprocess.

  Subclasses encapsulate subprocess lifecycle management and NDJSON streaming
  so callers can consume a uniform stream of event dicts regardless of the
  underlying execution mechanism.

  Template-method pattern: subclasses override ``_build_command()`` (required),
  and optionally ``_prepare_env()`` and ``translate_event()``.

  Event schema contract:
    All events yielded by run() must be JSON-serializable dicts with at
    minimum a "type" key. A "timestamp" (UTC ISO-8601) is recommended but
    optional — the persistence layer injects one if missing.
  """

  # Tunable lifecycle timeouts (class attrs so tests can monkey-patch).
  _POST_RESULT_TIMEOUT: float = 90.0
  _CLEANUP_TIMEOUT: float = 30.0

  def __init__(
      self,
      *,
      model: str | None = None,
      extra_flags: list[str] | None = None,
      buffer_limit: int | None = None,
      on_spawn: Callable[[int], Awaitable[None]] | None = None,
      instructions_content: str | None = None,
      resume_session_id: str | None = None,
      log_dir: Path | None = None,
      **_extra,
  ):
    self._model = model
    self._extra_flags = extra_flags or []
    self._buffer_limit = buffer_limit or DEFAULT_BUFFER_LIMIT
    self._on_spawn = on_spawn
    self._instructions_content = instructions_content
    self._resume_session_id = resume_session_id
    self._log_dir = log_dir
    self._proc: asyncio.subprocess.Process | None = None
    self._stderr_task: asyncio.Task | None = None
    self._stdin_task: asyncio.Task | None = None
    self._stderr_tail = bytearray()
    self.exit_code: int = -1
    self.stderr_text: str = ""
    # /proc/<pid>/stat field 22 captured right after spawn; Worker persists it
    # as ThreadMetadata.pid_start via the on_spawn callback.
    self.pid_start: str | None = None
    # Set when terminate() is called — deliberate user stop or shutdown.
    self.terminated: bool = False
    self.hang_diagnostics: dict | None = None

  def _effective_prompt(self, prompt: str) -> str:
    """Return prompt with instructions prepended, if any are configured."""
    if self._instructions_content:
      return f"<system-instructions>\n{self._instructions_content}\n</system-instructions>\n\n{prompt}"
    return prompt

  @staticmethod
  def _frame_system_prompt(system_prompt: str, prompt: str) -> str:
    """Frame a system prompt into the user prompt for backends lacking a system-prompt flag."""
    return f"<system-instructions>\n{system_prompt}\n</system-instructions>\n\n{prompt}"

  @abstractmethod
  def _build_command(self, prompt: str) -> list[str]:
    """Build the full CLI command list for the subprocess.

    This is the ONLY abstract method. Subclasses must implement it.
    """
    ...

  def _prepare_cwd(self, cwd: str) -> None:
    """Hook to prepare the working directory before subprocess spawn. No-op default."""

  def _write_instructions_file(self, cwd: str, filename: str, log_event: str) -> None:
    """Write _instructions_content into <cwd>/<filename> as UTF-8, or no-op when none is set.

    log_event stays caller-chosen so each backend keeps its own structured-log event name.
    """
    if not self._instructions_content:
      return
    path = Path(cwd) / filename
    path.write_text(self._instructions_content, encoding="utf-8")
    log.debug(log_event, path=str(path))

  def _prepare_env(self, env: dict) -> dict:
    """Hook to modify the environment before subprocess spawn. Identity default."""
    return env

  def _stdin_prompt(self, prompt: str) -> str | None:
    """Return prompt text to send on stdin, or None to keep prompt transport in argv."""
    return None

  def translate_event(self, event: dict) -> list[dict]:
    """Translate a raw backend NDJSON event into CC-compatible event dict(s).

    Each returned event dict MUST contain a "type" field. Standard types:
      - "assistant": message content (requires "message.content" list)
      - "system": system-level info
      - "result": final usage/cost summary
      - "error": error details

    Events MAY include a "timestamp" field (UTC ISO-8601 string). If omitted,
    the persistence layer (save_chat_event) injects one automatically.

    Identity default: returns [event] unchanged.
    """
    return [event]

  async def _spawn_piped_and_pin_identity(self, cmd: list[str], cwd: str, final_env: dict) -> None:
    """Spawn the child with piped stdio, pin (pid, pid_start), then fire on_spawn.

    Pipe-transport counterpart to run()'s raw-log spawn, for backends that read
    the child's stdout/stderr directly instead of tail-following log files.
    """
    self._proc = await asyncio.create_subprocess_exec(
        *cmd,
        cwd=cwd,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env=final_env,
        limit=self._buffer_limit,
        start_new_session=True,
    )
    # Pin the process identity BEFORE on_spawn so the callback can persist
    # (pid, pid_start) together; a proc that exited before we could read its
    # stat simply yields None and can never be judged alive later.
    stat_pair = runs.read_pid_stat(self._proc.pid)
    self.pid_start = stat_pair[0] if stat_pair else None
    if self._on_spawn is not None:
      await self._on_spawn(self._proc.pid)

  async def run(self, prompt: str, cwd: str, env: dict) -> AsyncIterator[dict]:
    """Spawn the agent subprocess and yield parsed NDJSON event dicts.

    Template method: calls _build_command() -> _prepare_env() -> subprocess
    spawn with stdout/stderr redirected to the raw log files -> tail-follow
    read loop -> translate_event() -> wait for exit.

    After the generator is fully consumed, ``exit_code`` and ``stderr_text``
    are populated.
    """
    await asyncio.to_thread(self._prepare_cwd, cwd)
    cmd = self._build_command(prompt)
    stdin_prompt = self._stdin_prompt(prompt)
    final_env = self._prepare_env(env)

    # The raw log files are the run's transport. When the caller gave no log
    # dir (one-shot use), use a throwaway dir so every covered backend shares
    # exactly one transport.
    temp_log_dir: Path | None = None
    if self._log_dir is not None:
      log_dir = self._log_dir
    else:
      temp_log_dir = Path(tempfile.mkdtemp(prefix="charliebot-run-"))
      log_dir = temp_log_dir
    log_dir.mkdir(parents=True, exist_ok=True)
    raw_path = log_dir / runs.RAW_LOG_NAME
    stderr_path = log_dir / runs.STDERR_LOG_NAME
    cursor_path = log_dir / runs.CURSOR_NAME

    # Fresh-spawn invariant: never read or append to a previous attempt's bytes.
    _rotate_stale_transport(log_dir, raw_path, stderr_path, cursor_path)

    # Open as real FDs and hand them to the child: after spawn the writer
    # needs no reader, so the run survives this process dying.
    raw_fd = os.open(raw_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    try:
      stderr_fd = os.open(stderr_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    except Exception:
      os.close(raw_fd)
      raise
    try:
      self._proc = await asyncio.create_subprocess_exec(
          *cmd,
          cwd=cwd,
          stdin=asyncio.subprocess.PIPE if stdin_prompt is not None else asyncio.subprocess.DEVNULL,
          stdout=raw_fd,
          stderr=stderr_fd,
          env=final_env,
          start_new_session=True,
      )
    finally:
      # The child holds its own copies of both fds.
      os.close(raw_fd)
      os.close(stderr_fd)

    # Pin the process identity BEFORE on_spawn so the callback can persist
    # (pid, pid_start) together; a proc that exited before we could read its
    # stat simply yields None and can never be judged alive later.
    stat_pair = runs.read_pid_stat(self._proc.pid)
    self.pid_start = stat_pair[0] if stat_pair else None

    if stdin_prompt is not None:
      self._stdin_task = asyncio.create_task(self._write_stdin_prompt(stdin_prompt))
    if self._on_spawn is not None:
      await self._on_spawn(self._proc.pid)

    try:
      async for event in tail_follow_events(
          raw_path,
          translate=self.translate_event,
          is_alive=lambda: self._proc is not None and self._proc.returncode is None,
          cursor=cursor_path,
          start_offset=0,
          post_result_timeout=self._POST_RESULT_TIMEOUT,
      ):
        yield event
      await self._wait_for_exit_and_cleanup(self._CLEANUP_TIMEOUT, stderr_path)
    finally:
      if temp_log_dir is not None:
        shutil.rmtree(temp_log_dir, ignore_errors=True)

  async def _write_stdin_prompt(self, prompt: str) -> None:
    """Write prompt to subprocess stdin while stdout is being drained."""
    assert self._proc is not None and self._proc.stdin is not None
    stdin = self._proc.stdin
    try:
      stdin.write(prompt.encode("utf-8"))
      await stdin.drain()
    finally:
      stdin.close()
      await stdin.wait_closed()

  async def _graceful_shutdown(
      self,
      timeout: float,
      *,
      timeout_log_event: str,
      wait_even_if_sigterm_not_sent: bool = False,
  ) -> None:
    """Send SIGTERM, wait for exit, and escalate to SIGKILL if the process does not exit in time."""
    assert self._proc is not None
    sigterm_sent = kill_process_group(self._proc.pid, signal.SIGTERM)
    if not sigterm_sent and not wait_even_if_sigterm_not_sent:
      return
    try:
      await asyncio.wait_for(self._proc.wait(), timeout=timeout)
    except TimeoutError:
      log.warning(timeout_log_event, pid=self._proc.pid)
      kill_process_group(self._proc.pid, signal.SIGKILL)

  async def _stream_stderr(self, stderr_log_path: Path | None) -> None:
    """Continuously read subprocess stderr; tee to <log_dir>/stderr.log and a 64 KB tail buffer.

    Streams live so `tail -f stderr.log` works during long runs and the in-memory
    tail is always up to date for `self.stderr_text`.
    """
    assert self._proc is not None and self._proc.stderr is not None
    stderr_log_cm = (
        aiofiles.open(stderr_log_path, "wb") if stderr_log_path is not None else contextlib.nullcontext(None))
    async with stderr_log_cm as stderr_log:
      while True:
        chunk = await self._proc.stderr.read(8192)
        if not chunk:
          break
        if stderr_log is not None:
          await stderr_log.write(chunk)
          await stderr_log.flush()
        self._stderr_tail.extend(chunk)
        if len(self._stderr_tail) > _STDERR_TAIL_BYTES:
          del self._stderr_tail[:len(self._stderr_tail) - _STDERR_TAIL_BYTES]

  async def _finish_stdin_task(self, timeout: float) -> Exception | None:
    """Flush the stdin-prompt writer; return its error (if any) for the caller to raise."""
    if self._stdin_task is None:
      return None
    assert self._proc is not None
    stdin_error: Exception | None = None
    try:
      await asyncio.wait_for(asyncio.shield(self._stdin_task), timeout=timeout)
    except TimeoutError as e:
      stdin_error = e
      log.warning("backend_stdin_write_timeout", pid=self._proc.pid, timeout=timeout)
      self._stdin_task.cancel()
      try:
        await self._stdin_task
      except asyncio.CancelledError:
        log.debug("backend_stdin_write_cancelled", pid=self._proc.pid)
      except Exception as cancel_error:
        stdin_error = cancel_error
    except Exception as e:
      stdin_error = e
    return stdin_error

  async def _wait_for_proc_exit(self, timeout: float) -> None:
    """Wait for the subprocess to exit; on a hang capture diagnostics and escalate to SIGKILL."""
    assert self._proc is not None
    try:
      await asyncio.wait_for(self._proc.wait(), timeout=timeout)
    except TimeoutError:
      log.warning("backend_wait_timeout_after_result", pid=self._proc.pid)
      self.hang_diagnostics = await _capture_proc_diagnostics(self._proc.pid)
      await self._graceful_shutdown(
          timeout,
          timeout_log_event="backend_sigkill_after_result",
          wait_even_if_sigterm_not_sent=True,
      )

  async def _wait_for_exit_and_cleanup(self, timeout: float, stderr_path: Path) -> None:
    """Raw-transport cleanup: finish stdin, wait for process exit, escalate on hang.

    Companion to the tail-follow read loop: stdout/stderr are file-descriptor
    redirected, so there is no stream reader to drain — the process may simply
    outlive us, hence the diagnostics + SIGKILL escalation on a hang, and
    ``stderr_text`` comes back from the stderr log file's tail.
    """
    assert self._proc is not None
    stdin_error = await self._finish_stdin_task(timeout)
    await self._wait_for_proc_exit(timeout)
    self.exit_code = self._proc.returncode or 0
    self.stderr_text = _read_stderr_tail(stderr_path)
    if stdin_error is not None:
      raise stdin_error

  async def _drain_and_cleanup(self, timeout: float) -> None:
    """Wait for stderr-streamer + process exit; capture diagnostics + escalate to kill on hang."""
    assert self._proc is not None
    stdin_error = await self._finish_stdin_task(timeout)
    if self._stderr_task is not None:
      try:
        await asyncio.wait_for(asyncio.shield(self._stderr_task), timeout=timeout)
      except TimeoutError:
        log.debug("backend_stderr_stream_timeout", pid=self._proc.pid, timeout=timeout)
    await self._wait_for_proc_exit(timeout)
    if self._stderr_task is not None and not self._stderr_task.done():
      self._stderr_task.cancel()
      try:
        await self._stderr_task
      except asyncio.CancelledError:
        log.debug("backend_stderr_stream_cancelled", pid=self._proc.pid)
      except Exception as e:
        log.warning("backend_stderr_stream_cancel_failed", pid=self._proc.pid, error=str(e))
    self.exit_code = self._proc.returncode or 0
    self.stderr_text = bytes(self._stderr_tail).decode("utf-8", errors="replace").strip()
    if stdin_error is not None:
      raise stdin_error

  async def one_shot_text(self, prompt: str, system_prompt: str, *, timeout: float) -> str:
    """Generate text for a single prompt with no tools and no instructions file.

    Backend-agnostic default: run the agent in a throwaway empty cwd (so no
    AGENTS.md/CLAUDE.md is written) with the system prompt framed into the user
    prompt, collecting assistant text until the RESULT event. Subclasses with a
    cheaper CLI-native one-shot (claude/codex/opencode) override this.
    """
    from src.core.message_aggregator import extract_text_from_message

    # A one-shot writes no instructions file; the framed system prompt is the only steering.
    self._instructions_content = None
    cwd = tempfile.mkdtemp(prefix="charliebot-oneshot-")
    env = dict(os.environ)
    framed = self._frame_system_prompt(system_prompt, prompt)

    async def _collect() -> str:
      parts: list[str] = []
      seen_result = False
      async for event in self.run(framed, cwd, env):
        evt_type = event.get("type")
        if not seen_result and evt_type == ET.ASSISTANT:
          parts.append(extract_text_from_message(event.get("message")))
        elif evt_type == ET.RESULT:
          seen_result = True
      return "".join(parts).strip()

    try:
      return await asyncio.wait_for(_collect(), timeout)
    except TimeoutError:
      await self.terminate()
      raise
    finally:
      shutil.rmtree(cwd, ignore_errors=True)

  async def terminate(self) -> None:
    """Send SIGTERM to process group; escalate to SIGKILL if not exited within 5 s."""
    self.terminated = True
    if self._proc is None or self._proc.returncode is not None:
      return
    await self._graceful_shutdown(5.0, timeout_log_event="backend_terminate_timeout")

  def detach(self) -> None:
    """Forget the running child without signalling it (graceful-shutdown let-go).

    The asyncio subprocess transport kills a still-running child when closed
    (BaseSubprocessTransport.close), and every transport is closed at
    event-loop teardown — so "no signal at shutdown" only holds if the
    transport's process handle is dropped first. The run's ground truth stays
    on disk (raw log + cursor + pid/pid_start); the next boot re-attaches.
    """
    proc = self._proc
    if proc is None:
      return
    transport = getattr(proc, "_transport", None)
    if transport is not None and getattr(transport, "_proc", None) is not None:
      transport._proc = None
    self._proc = None
