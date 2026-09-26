"""Run truth on disk for the restart-safe agent runtime.

A worker/master run's ground truth lives in its thread data directory, not in
the server process: the agent subprocess writes its NDJSON stdout straight to
``agent.raw.ndjson`` and stderr to ``agent.stderr.log`` via inherited file
descriptors, and the server is a tail-follow consumer that can die and
re-attach at a recorded byte offset (``agent.raw.cursor``) without the writer
noticing.

This module owns the pure/queryable parts of that contract:

- path derivation for every per-run file;
- process liveness (``pid`` + ``/proc/<pid>/stat`` field 22 + host boot time);
- descendant discovery (one ``/proc/*/fd/1`` scan, diagnostic only — never a
  liveness input);
- the outcome table mapping on-disk facts to a ``RunOutcome``;
- the pure raw-line -> translated-event projection shared by the live read
  loop, the re-attach path, and tests;
- the end-of-run error-hint selection for a failed invocation (the
  invocation's own error events over the stderr tail);
- reading the run's true completion time (the raw log's final mtime).
"""

from __future__ import annotations

import asyncio
import os
import re
import signal
import stat
import time
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from datetime import UTC, datetime
from enum import StrEnum
from pathlib import Path
from typing import TYPE_CHECKING

import orjson

from src.core import event_types as ET
from src.core.constants import BackendType
from src.core.control_events import ACTOR_SYSTEM, ControlEventSink, build_control_event, sha256_hex, stable_run_id
from src.core.json_utils import atomic_write_text
from src.core.models import RunRecord, ensure_utc, utc_now
from src.core.ndjson import parse_ndjson_line
from src.core.run_token import b64url_decode, b64url_encode
from src.core.session_aliases import SessionAliasStore
from src.core.sidebar_state import mark_sidebar_dirty
from src.core.timeouts import NO_OUTPUT_REPORT_THRESHOLD

if TYPE_CHECKING:
  from src.core.config import CharlieBotConfig

RAW_LOG_NAME = "agent.raw.ndjson"
STDERR_LOG_NAME = "agent.stderr.log"
CURSOR_NAME = "agent.raw.cursor"

# The transport directory every per-run file sits in (a thread's data dir, a
# master run's <session>/data/master_runs/<started_at> dir) and the
# master-capture directory name under a session's data dir. Writers (the
# master turn in src/agents, threads.py's creation skeleton) and readers
# (token_tally's corpus walk, storage_cool's transport sweep) must agree on
# these names.
DATA_DIR_NAME = "data"
MASTER_RUNS_DIR_NAME = "master_runs"

# Backend types whose event transport does not go through the shared base read
# loop (opencode serves events over its own HTTP SSE; antigravity and tui-cli
# manage their own pipes). A restart cannot attach to those, so an interrupted
# run on one of them still fails — but with this explicit reason, never
# disguised as a crash.
UNCOVERED_BACKEND_TYPES: frozenset[BackendType] = frozenset(
    {BackendType.OPENCODE, BackendType.ANTIGRAVITY, BackendType.TUI_CLI})
TRANSPORT_NOT_COVERED_REASON = "backend transport not covered by restart-safe runtime"

LEGACY_RAW_MISSING_REASON = "raw log missing (run predates restart-safe transport)"
DIED_WITHOUT_RESULT_REASON = "process exited without a final result event"

# Effective-alive verdicts: wherever death cannot be PROVEN (a liveness input
# is missing, or the probe says alive), the run is treated as alive and never
# finalized failed on missing evidence. These reasons route the boot-recovery
# passes' report-only branch (no re-attach) for rows that have nothing followable.
UNCOVERED_ALIVE_REASON = "uncovered-alive"
RAW_MISSING_ALIVE_REASON = "raw-missing-alive"

# Improve-loop iteration threads are identified by their description prefix;
# the loop task itself does not survive a restart (loop continuation is an
# explicit non-goal), so these threads are finalized, never respawned, and
# the shutdown path terminates their processes along with the loop. The
# description producer (improve_command.py) builds the prefix from this
# constant, so producer and matchers cannot drift.
IMPROVE_ITERATION_PREFIX = "Iterative improvement — iteration"

# Pids that must never appear in a kill list derived from the fd scan.
_NEVER_KILL_PIDS = frozenset({0, 1})


def backend_type(cfg: CharlieBotConfig, backend_id: str | None) -> str | None:
  """The configured transport type of ``backend_id``; None when unset or unknown."""
  if not backend_id:
    return None
  option = cfg.get_backend_option(backend_id)
  return option.type if option else None


class RunOutcome(StrEnum):
  """Outcome rows for an interrupted run."""
  COMPLETED = "completed"  # last result event exists -> finalize from it
  RUNNING = "running"  # alive and producing output -> re-attach
  DIED = "died"  # gone without a result event -> fail, keep worktree
  STALLED = "stalled"  # alive but raw log silent beyond the report threshold
  NEVER_STARTED = "never_started"  # registered but never spawned -> respawn


@dataclass(frozen=True)
class HolderProcess:
  """A process whose stdout fd still points at a run's raw log (inherited fd)."""
  pid: int
  cmdline: str


@dataclass(frozen=True)
class RunResolution:
  """Result of resolving an interrupted run from on-disk facts."""
  outcome: RunOutcome
  reason: str = ""
  # Raw log's final mtime — the run's true completion time, independent of
  # downtime. Present whenever the raw log exists.
  completed_at: datetime | None = None
  # Descendants that outlived the run while holding its raw-log fd.
  leftover_holders: tuple[HolderProcess, ...] = ()


# ---------------------------------------------------------------------------
# Path derivation
# ---------------------------------------------------------------------------


def raw_log_path(thread_dir: Path) -> Path:
  return thread_dir / DATA_DIR_NAME / RAW_LOG_NAME


def stderr_log_path(thread_dir: Path) -> Path:
  return thread_dir / DATA_DIR_NAME / STDERR_LOG_NAME


def cursor_path(thread_dir: Path) -> Path:
  return thread_dir / DATA_DIR_NAME / CURSOR_NAME


def master_run_log_dir(session_dir: Path, started_at: datetime) -> Path:
  """The per-turn transport dir one master run pins its raw log, stderr log, and cursor in."""
  return session_dir / DATA_DIR_NAME / MASTER_RUNS_DIR_NAME / started_at.isoformat()


# ---------------------------------------------------------------------------
# Process liveness
# ---------------------------------------------------------------------------


def read_host_boot_time() -> datetime:
  """Host boot time from /proc/stat ``btime`` as a tz-aware UTC datetime."""
  with open("/proc/stat", encoding="utf-8") as f:
    for line in f:
      if line.startswith("btime "):
        return datetime.fromtimestamp(int(line.split()[1]), tz=UTC)
  raise RuntimeError("/proc/stat missing btime line")


def read_pid_stat(pid: int) -> tuple[str, str] | None:
  """Return (start_time_field, state) from /proc/<pid>/stat, or None when gone.

  Field 22 (process start time) is a u64 in kernel clock ticks; keep it as the
  raw string so comparisons are exact and overflow-free. The comm field may
  contain spaces and closing parens, so fields are split after the LAST ')':
  field 3 (state) is then index 0 and field 22 is index 19.
  """
  try:
    with open(f"/proc/{pid}/stat", encoding="utf-8") as f:
      content = f.read()
  except OSError:
    return None
  rest = content.rpartition(")")[2].split()
  if len(rest) < 20:
    return None
  return rest[19], rest[0]


def is_run_alive(
    pid: int | None,
    pid_start: str | None,
    started_at: datetime | None,
    host_boot_time: datetime,
) -> bool:
  """Whether the recorded (pid, pid_start) pair still points at a live process.

  Three conjuncts pin one process instance: /proc/<pid>/stat exists and its
  field 22 equals the recorded pid_start (pid reuse cannot fake it), the
  process is not a zombie, and the run began after the host's current boot
  (nothing survives a host reboot, so a pre-boot pair is always stale).
  Descendants that inherited the raw-log fd do NOT count as liveness — they
  are reported via the fd scan instead.
  """
  if pid is None or pid_start is None or started_at is None:
    return False
  stat_pair = read_pid_stat(pid)
  if stat_pair is None:
    return False
  current_start, state = stat_pair
  if current_start != pid_start or state == "Z":
    return False
  if started_at.tzinfo is None:
    raise ValueError("started_at must be timezone-aware")
  return started_at.astimezone(UTC) > host_boot_time


def run_alive_probe(
    pid: int | None,
    pid_start: str | None,
    started_at: datetime | None,
    host_boot_time: datetime,
) -> Callable[[], bool]:
  """The re-evaluable form of ``is_run_alive``: each call re-runs the judgment.

  kill_group_escalating re-probes between signals, so kill authorization needs
  the recorded-identity judgment as a callable, not a one-shot boolean.
  """
  return lambda: is_run_alive(pid, pid_start, started_at, host_boot_time)


# ---------------------------------------------------------------------------
# Descendant discovery (diagnostic only — never a liveness input)
# ---------------------------------------------------------------------------


def scan_stdout_holders() -> dict[tuple[int, int], list[HolderProcess]]:
  """Map (st_dev, st_ino) -> processes whose fd 1 points at that regular file.

  One ``/proc/*/fd/1`` scan (~3 ms for ~500 processes). fd 1 follows the
  symlink to whatever the process's stdout is; only regular files are indexed
  (a run's raw log; consoles/pipes/sockets are skipped). Used exclusively to
  find descendants that outlived their run's process group — it feeds reporting
  and the leftover-holder cleanup, never liveness judgments.
  """
  holders: dict[tuple[int, int], list[HolderProcess]] = {}
  for entry in os.scandir("/proc"):
    if not entry.name.isdigit():
      continue
    pid = int(entry.name)
    try:
      st = os.stat(f"/proc/{pid}/fd/1")  # follows the fd symlink
    except OSError:
      continue  # process exited mid-scan, or no fd 1 / no permission
    if not stat.S_ISREG(st.st_mode):
      continue
    try:
      with open(f"/proc/{pid}/cmdline", "rb") as f:
        cmdline = f.read().replace(b"\0", b" ").decode("utf-8", errors="replace").strip()
    except OSError:
      cmdline = ""
    holders.setdefault((st.st_dev, st.st_ino), []).append(HolderProcess(pid=pid, cmdline=cmdline))
  return holders


def leftover_holders_for(
    raw_path: Path,
    holders_scan: dict[tuple[int, int], list[HolderProcess]],
    *,
    run_pid: int | None,
) -> tuple[HolderProcess, ...]:
  """Descendants still holding *raw_path*'s inode, excluding the run itself.

  ``run_pid`` is the recorded (dead or re-used) leader pid; the current server
  pid and init/system pids are always excluded so a kill list built from this
  can never hit either.
  """
  try:
    st = raw_path.stat()
  except OSError:
    return ()
  own = os.getpid()
  out = []
  for holder in holders_scan.get((st.st_dev, st.st_ino), []):
    if holder.pid in (run_pid, own) or holder.pid in _NEVER_KILL_PIDS:
      continue
    out.append(holder)
  return tuple(out)


# ---------------------------------------------------------------------------
# Raw -> event projection (pure)
# ---------------------------------------------------------------------------


def parse_raw_lines(raw_bytes: bytes) -> list[dict]:
  """Parse raw log bytes into event dicts, skipping blank/torn lines.

  A trailing partial line (written by a process killed mid-write) is dropped —
  its offset stays un-consumed semantics make re-reading it produce at most a
  duplicate, never a loss.
  """
  # Lines ride zero-copy memoryview slices: a bytes slice copies, and the copy
  # is parse inflation at the multi-MB line a tool-result-heavy turn produces
  # (a 10 MB line measured ~14 ms parsed from a view against ~27 ms through
  # the copy, the same floor the M84 tail-follow walk documented). Valid lines
  # parse straight off the slice; a rejected line takes the reader skip
  # contract's verdict (blank invisible, malformed logged, a torn multibyte
  # char parsing as U+FFFD) instead of an inline duplicate of it.
  events: list[dict] = []
  view = memoryview(raw_bytes)
  start = 0
  size = len(raw_bytes)
  find = raw_bytes.find
  while start < size:
    end = find(b"\n", start)
    stop = size if end == -1 else end
    piece = view[start:stop]
    start = stop + 1
    try:
      events.append(orjson.loads(piece))
    except ValueError:
      event = parse_ndjson_line(piece, log_event="raw_line_not_json", log_fields={})
      if event is not None:
        events.append(event)
  return events


def project_raw_events(
    events: Iterable[dict],
    translate: Callable[[dict], list[dict]],
) -> list[dict]:
  """Pure projection: apply *translate* to every raw event in order.

  ``translate`` is stateful for some backends (codex text buffering, gemini
  buffers), so callers must pass a FRESH backend's method for a whole-file
  scan; the live read loop uses the one instance for the run's lifetime.
  """
  out: list[dict] = []
  for event in events:
    out.extend(translate(event))
  return out


def summarize_result(events: Iterable[dict]) -> dict | None:
  """The last ``result`` event in a projected event stream, if any."""
  last: dict | None = None
  for event in events:
    if event.get("type") == ET.RESULT:
      last = event
  return last


def result_success(result: dict) -> bool:
  """Terminal-status judgment from a result event (matches spawner semantics)."""
  return result.get("subtype") in (None, "success") and result.get("is_error") in (None, False)


# The stderr fallback hint keeps the stderr-first slice's 500-character bound
# (the behavior it replaces); the structured error event passes through whole.
_STDERR_HINT_MAX_CHARS = 500

# ANSI escape sequences (CSI parameters/intermediates/final byte, OSC up to its
# BEL or ST terminator) render as garbage in a chat hint; the control-character
# class sweeps whatever escape forms remain. Newlines and tabs survive: they
# are stderr formatting, not noise.
_ANSI_ESCAPE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
_CONTROL_CHAR = re.compile(r"[\x00-\x08\x0b-\x1f\x7f-\x9f]")


def clean_control_characters(text: str) -> str:
  """Strip ANSI escapes and control characters, keeping newlines and tabs."""
  return _CONTROL_CHAR.sub("", _ANSI_ESCAPE.sub("", text))


def select_error_hint(error_messages: list[str], stderr_text: str) -> str | None:
  """End-of-run error hint for one failed invocation: its own error events first.

  A failed invocation's translated ``error`` events carry the real failure (an
  HTTP status and reason); the stderr tail usually carries only the client
  library's generic help banner. The last non-empty error message wins; when
  the invocation emitted none, the stderr tail is the fallback — control
  characters cleaned, capped at the bound the stderr-first slice it replaces
  used. None when neither channel says anything. Pure: both end paths (live
  tracking and restart re-attach) feed it from what they already hold, and the
  raw log itself is never touched.
  """
  for message in reversed(error_messages):
    if message.strip():
      return message
  cleaned = clean_control_characters(stderr_text).strip()
  return cleaned[:_STDERR_HINT_MAX_CHARS] or None


def project_raw_file(raw_path: Path, translate: Callable[[dict], list[dict]]) -> list[dict]:
  """Whole-file projection: read, parse, and project one raw log.

  ``translate`` must be fresh (see project_raw_events). The scan is a full
  read+parse of the log's bytes, so event-loop callers reach it through
  asyncio.to_thread.
  """
  return project_raw_events(parse_raw_lines(raw_path.read_bytes()), translate)


def scan_result_exit(
    raw_path: Path,
    translate: Callable[[dict], list[dict]],
) -> tuple[list[dict], dict | None, int]:
  """Whole-file result scan: projected events, last result event, and its exit code.

  A missing raw log is a legal drain input (never-started run, legacy thread
  without the transport): it scans to no events, no result, exit -1. The
  whole-file scan needs a FRESH translate — a stateful translate may not be
  reused after it consumed a stream tail. Exit code: 0 only when a result
  event exists and ``result_success`` holds, else -1, the code a died-mid-run
  live turn reports for its missing result event.
  """
  events = project_raw_file(raw_path, translate) if raw_path.is_file() else []
  result = summarize_result(events)
  exit_code = 0 if result is not None and result_success(result) else -1
  return events, result, exit_code


# ---------------------------------------------------------------------------
# Completion time and cursor
# ---------------------------------------------------------------------------


def raw_completion_time(raw_path: Path) -> datetime | None:
  """The raw log's final mtime as a tz-aware UTC datetime (None when missing).

  Backend- and content-agnostic: how long the server was down does not affect
  it, so a run that finished during downtime gets its true completion time.
  """
  try:
    st = raw_path.stat()
  except OSError:
    return None
  return datetime.fromtimestamp(st.st_mtime, tz=UTC)


def read_raw_cursor(cursor: Path) -> int:
  """Consumed byte offset; 0 (replay) on a missing or unparseable cursor file."""
  try:
    return int(cursor.read_text(encoding="utf-8").strip())
  except (OSError, ValueError):
    return 0


CURSOR_FIELD_BYTES = 20


class RawCursorWriter:
  """Held-fd checkpoint writer for one tail-follow mount's cursor file.

  The follow checkpoints once per consumed line, so the write must not pay an
  open+truncate+close cycle per call (~0.9 ms on this host's storage): the
  mount holds one fd and rewrites the offset as one fixed-width zero-padded
  decimal in place. The fixed width keeps every rewrite a single pwrite, so a
  read can only observe the full old or the full new value; a torn read
  between two monotonic offsets parses to the older one and replays
  duplicates, never loss (the read_raw_cursor contract).
  """

  def __init__(self, path: Path) -> None:
    self._path = path
    self._fd: int | None = None

  def write(self, offset: int) -> None:
    if self._fd is None:
      self._path.parent.mkdir(parents=True, exist_ok=True)
      self._fd = os.open(self._path, os.O_WRONLY | os.O_CREAT, 0o666)
    os.pwrite(self._fd, b"%0*d" % (CURSOR_FIELD_BYTES, offset), 0)

  def close(self) -> None:
    if self._fd is not None:
      os.close(self._fd)
      self._fd = None


# ---------------------------------------------------------------------------
# Outcome resolution
# ---------------------------------------------------------------------------


def _missing_liveness_fields(
    pid: int | None,
    pid_start: str | None,
    started_at: datetime | None,
) -> list[str]:
  """Names of the liveness inputs that are absent, in fixed order."""
  missing = []
  if pid is None:
    missing.append("pid")
  if pid_start is None:
    missing.append("pid_start")
  if started_at is None:
    missing.append("started_at")
  return missing


def resolve_run(
    *,
    raw_path: Path,
    pid: int | None,
    pid_start: str | None,
    started_at: datetime | None,
    backend_type: str | None,
    translate: Callable[[dict], list[dict]],
    host_boot_time: datetime,
    holders_scan: dict[tuple[int, int], list[HolderProcess]] | None = None,
) -> RunResolution:
  """Resolve an interrupted run's outcome purely from on-disk facts.

  Row order matters: registered-but-never-spawned is judged before backend
  coverage (a fresh spawn works for any backend), and coverage is judged
  before semantics that need the shared read loop's artifacts.

  One invariant governs every row below: death is reported only when it can be
  PROVEN — pid, pid_start, and started_at all present AND ``is_run_alive``
  says dead. Anything else (any input missing so death is unverifiable, or the
  probe says alive) is treated as alive and resolves to a RUNNING/STALLED row,
  never a DIED-on-missing-evidence finalize.

  ``holders_scan`` is the output of one ``scan_stdout_holders`` call shared by
  a whole reconcile pass; when given and the run is not alive, leftover
  descendants are attached to the resolution (the outcome itself still comes
  from the other rows).
  """
  now = datetime.now(UTC)
  raw_exists = raw_path.is_file()

  if not raw_exists and pid is None:
    return RunResolution(outcome=RunOutcome.NEVER_STARTED)

  missing = _missing_liveness_fields(pid, pid_start, started_at)
  missing_note = f"missing liveness field(s): {', '.join(missing)}" if missing else ""
  alive = is_run_alive(pid, pid_start, started_at, host_boot_time)
  # Effective-alive, defined once for the whole table: verified-alive, or death
  # unverifiable because a liveness input is missing. Both mean "treat as alive".
  effectively_alive = alive or bool(missing)

  completed_at: datetime | None = None
  result: dict | None = None
  if raw_exists:
    completed_at = raw_completion_time(raw_path)
    events = project_raw_file(raw_path, translate)
    result = summarize_result(events)

  if backend_type in UNCOVERED_BACKEND_TYPES and result is None:
    # A result event already on disk falls through to the downstream result
    # row and completes normally; this row only judges runs without one.
    if effectively_alive:
      return RunResolution(outcome=RunOutcome.RUNNING, reason=UNCOVERED_ALIVE_REASON, completed_at=completed_at)
    return RunResolution(outcome=RunOutcome.DIED, reason=TRANSPORT_NOT_COVERED_REASON, completed_at=completed_at)
  if not raw_exists:
    if effectively_alive:
      return RunResolution(outcome=RunOutcome.RUNNING, reason=RAW_MISSING_ALIVE_REASON)
    return RunResolution(outcome=RunOutcome.DIED, reason=LEGACY_RAW_MISSING_REASON)

  leftovers: tuple[HolderProcess, ...] = ()
  if not alive and holders_scan is not None:
    leftovers = leftover_holders_for(raw_path, holders_scan, run_pid=pid)

  if result is not None:
    return RunResolution(
        outcome=RunOutcome.COMPLETED,
        completed_at=completed_at,
        leftover_holders=leftovers,
    )
  if effectively_alive:
    silent_for = (now - completed_at).total_seconds() if completed_at else 0.0
    if silent_for > NO_OUTPUT_REPORT_THRESHOLD:
      reason = (f"alive but no raw output for {int(silent_for)}s "
                f"(>{NO_OUTPUT_REPORT_THRESHOLD}s threshold)")
      if missing_note:
        reason = f"{reason}; {missing_note}"
      return RunResolution(
          outcome=RunOutcome.STALLED,
          reason=reason,
          completed_at=completed_at,
      )
    return RunResolution(outcome=RunOutcome.RUNNING, reason=missing_note, completed_at=completed_at)
  return RunResolution(
      outcome=RunOutcome.DIED,
      reason=DIED_WITHOUT_RESULT_REASON,
      completed_at=completed_at,
      leftover_holders=leftovers,
  )


# ---------------------------------------------------------------------------
# Run records (schema_version=2)
# ---------------------------------------------------------------------------
# A v2 Run lives at sessions/<id>/data/runs/<run_id>/ with its metadata.json,
# its pinned task-spec body, and (once an execution adapter launches it) the
# same raw transport files the master-run dir carries. The store below is the
# single owner of run metadata and terminal facts: registration, retry
# binding, durable stop requests, and the run_finished write all funnel
# through it, so a natural finish and a cancellation race resolve to whichever
# terminal fact landed first and a restart's fresh readers read the same
# durable request and result.

RUNS_DIR_NAME = "runs"
RUN_METADATA_NAME = "metadata.json"
RUN_TASK_SPEC_NAME = "task_spec.md"

# Bounded wait after SIGTERM for the actual exit the interrupted fact requires.
STOP_EXIT_POLL_SECONDS = 0.05
STOP_EXIT_WAIT_SECONDS = 10.0

# The caller-identity refusal reasons shared by the API dependency and the CLI's
# run-scoped query resolution — one active-Run predicate, never a second.
RUN_IDENTITY_UNKNOWN_DETAIL = "run token does not reference an active run"
RUN_IDENTITY_NOT_LAUNCHED_DETAIL = "run token references a run that has not launched"


def run_identity_refusal(run: RunRecord | None, events: list[dict]) -> str | None:
  """Why *run* is not an active, launched Run for caller identity, or None.

    The one predicate both identity consumers share: the API caller-identity
    dependency and the CLI's run-scoped query resolution. A run token stands
    only for a registered Run without a terminal fact whose launch identity
    (pid, pid_start) is pinned.
    """
  if run is None:
    return RUN_IDENTITY_UNKNOWN_DETAIL
  for event in events:
    if event.get("type") == ET.RUN_FINISHED and event.get("run_id") == run.id:
      return RUN_IDENTITY_UNKNOWN_DETAIL
  if run.pid is None or run.pid_start is None:
    return RUN_IDENTITY_NOT_LAUNCHED_DETAIL
  return None


def terminal_outcome_in_events(events: list[dict], run_id: str) -> str | None:
  """The last recorded run_finished outcome of one Run (None while none).

  The pure fact scan behind ``RunStore.terminal_outcome``: the API's legacy
  status fold (src.api.threads) reads the same durable events through this
  function so both owners answer one identical question.
  """
  outcome: str | None = None
  for event in events:
    if event.get("type") == ET.RUN_FINISHED and event.get("run_id") == run_id:
      outcome = event.get("outcome")
  return outcome


def stop_requested_in_events(
    events: list[dict],
    run_id: str,
    request_id: str | None = None,
) -> bool:
  """Whether a durable run_stop_requested fact exists (optionally one request_id's).

  The pure fact scan behind ``RunStore.stop_requested``: the work-state
  derivation (src.core.task_sessions) reads the same durable events through
  this function so both owners answer one identical question.
  """
  for event in events:
    if event.get("type") != ET.RUN_STOP_REQUESTED or event.get("run_id") != run_id:
      continue
    if request_id is None or event.get("request_id") == request_id:
      return True
  return False


class RunNotFoundError(LookupError):
  """The requested run record does not exist (API: 404)."""


# The task-tree execution and completion paths raise this sentence inside
# their own miss types, and the run-status route (src/api/sessions.py)
# restates it verbatim as the client-visible 404 detail; one home keeps the
# layers from drifting apart on the wording.
def run_not_found_in_task_text(run_id: str, session_id: str) -> str:
  """The task-tree run-miss sentence (API: 404 detail)."""
  return f"run {run_id} not found in task {session_id}"


class RunIdentityConflictError(Exception):
  """The recorded (pid, pid_start) pair no longer names one process instance (API: 409).

  The durable stop request stays on disk when this raises — it is the pending
  evidence a fresh recovery reader re-checks against the same identity rule.
  """


class RunInputMismatchError(ValueError):
  """A finish acknowledgement payload that does not match the run's registered batch."""


@dataclass(frozen=True)
class RunStopResult:
  """The outcome of one stop request against one run."""
  run_id: str
  stop_requested: bool
  # None while the request is only durably recorded — never a completion assertion.
  outcome: str | None


@dataclass(frozen=True)
class RunPageSlice:
  """One keyset page of a session's run records."""
  items: list[RunRecord]
  next_cursor: str | None


def _run_sort_key(run: RunRecord) -> tuple[datetime, str]:
  """Runs order chronologically by launch; a queued (never-launched) run sorts first."""
  started = run.started_at if run.started_at is not None else datetime.min.replace(tzinfo=UTC)
  return (started, run.id)


def _encode_run_cursor(key: tuple[datetime, str], descending: bool = False) -> str:
  """Opaque keyset cursor: base64url JSON of the (started_at, id) boundary.

  The page order rides inside the cursor so a cursor minted under one order
  can never be replayed against the other: the boundary pair alone is not
  order-aware, and a mismatched replay would silently skip or repeat rows.
  Pre-order cursors (no flag) decode as ascending, which keeps every cursor a
  current client already holds valid.
  """
  payload: dict = {"s": key[0].isoformat(), "i": key[1]}
  if descending:
    payload["d"] = True
  return b64url_encode(orjson.dumps(payload))


def _decode_run_cursor(cursor: str) -> tuple[tuple[datetime, str], bool]:
  """Decode one run-page cursor into its boundary and page order, failing loud
  on a malformed value."""
  try:
    payload = orjson.loads(b64url_decode(cursor))
    started = datetime.fromisoformat(payload["s"]) if payload["s"] else datetime.min.replace(tzinfo=UTC)
    return (ensure_utc(started), payload["i"]), bool(payload.get("d", False))
  except (ValueError, KeyError, TypeError) as e:
    raise ValueError(f"malformed run page cursor: {cursor!r}") from e


class RunStore:
  """Owns v2 run records, their aliases, and every terminal fact."""

  def __init__(
      self,
      cfg: CharlieBotConfig,
      control_lock: asyncio.Lock,
      events: ControlEventSink,
      aliases: SessionAliasStore,
  ) -> None:
    self._cfg = cfg
    self._lock = control_lock  # the one short control write lock, shared with the tree owner
    self._events = events
    self._aliases = aliases
    # Installed by the tree owner: the full fact-history reader (archived
    # segments included) the terminal/stop/identity reads use, so a rotated
    # acknowledgement or stop request never un-dones itself.
    self._fact_history_loader: Callable[[str], list[dict]] | None = None

  def set_fact_history_loader(self, loader: Callable[[str], list[dict]]) -> None:
    """Install the tree owner's full-history reader (archived segments included)."""
    self._fact_history_loader = loader

  # -- paths ---------------------------------------------------------------

  def runs_root(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id / DATA_DIR_NAME / RUNS_DIR_NAME

  def run_dir(self, session_id: str, run_id: str) -> Path:
    return self.runs_root(session_id) / run_id

  def metadata_path(self, session_id: str, run_id: str) -> Path:
    return self.run_dir(session_id, run_id) / RUN_METADATA_NAME

  def task_spec_path(self, session_id: str, run_id: str) -> Path:
    return self.run_dir(session_id, run_id) / RUN_TASK_SPEC_NAME

  # -- reads ---------------------------------------------------------------

  def read_run_sync(self, session_id: str, run_id: str) -> RunRecord | None:
    path = self.metadata_path(session_id, run_id)
    if not path.is_file():
      return None
    try:
      return RunRecord.model_validate_json(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
      raise RuntimeError(f"run metadata unreadable at {path}: {e}") from e

  async def get_run(self, session_id: str, run_id: str) -> RunRecord | None:
    return await asyncio.to_thread(self.read_run_sync, session_id, run_id)

  def _require_run(self, session_id: str, run_id: str) -> RunRecord:
    """The run's record, or RunNotFoundError when no run metadata exists.

    A plain read: no lock is taken here, so the under-lock write paths and
    the outside-lock recovery path share one read-or-raise.
    """
    run = self.read_run_sync(session_id, run_id)
    if run is None:
      raise RunNotFoundError(f"run {run_id} not found in session {session_id}")
    return run

  def list_run_records_sync(self, session_id: str) -> list[RunRecord]:
    """Every run record of one session, chronological (queued first)."""
    root = self.runs_root(session_id)
    if not root.is_dir():
      return []
    runs: list[RunRecord] = []
    for entry in root.iterdir():
      if not entry.is_dir():
        continue
      run = self.read_run_sync(session_id, entry.name)
      if run is not None:
        runs.append(run)
    runs.sort(key=_run_sort_key)
    return runs

  def list_runs_page_sync(
      self,
      session_id: str,
      limit: int,
      cursor: str | None,
      *,
      descending: bool = False,
  ) -> RunPageSlice:
    """One keyset page ordered by the canonical (started_at, id) launch order.

    The default stays chronological ascending (queued reservations first) with
    its original cursor semantics. ``descending`` is the same total order read
    backwards: the most recently started run opens the page, and queued
    (never-launched) reservations close it — so a one-row descending page IS
    the session's authoritative latest launch, at a request cost that never
    grows with history length. A cursor minted under one order is refused
    under the other instead of being silently misapplied.
    """
    runs = self.list_run_records_sync(session_id)
    runs.sort(key=_run_sort_key, reverse=descending)
    after = _decode_run_cursor(cursor) if cursor else None
    if after is not None:
      boundary, cursor_descending = after
      if cursor_descending != descending:
        raise ValueError(
            f"run page cursor was minted for {'descending' if cursor_descending else 'ascending'} "
            f"order and cannot be replayed under {'descending' if descending else 'ascending'} order")
      if descending:
        runs = [r for r in runs if _run_sort_key(r) < boundary]
      else:
        runs = [r for r in runs if _run_sort_key(r) > boundary]
    page = runs[:limit]
    next_cursor = (_encode_run_cursor(_run_sort_key(page[-1]), descending) if len(runs) > limit and page else None)
    return RunPageSlice(items=page, next_cursor=next_cursor)

  # -- facts ---------------------------------------------------------------

  def load_events_sync(self, session_id: str) -> list[dict]:
    """The session's durable fact history (archived segments included once the
    tree owner installs the reader; the live log before that)."""
    if self._fact_history_loader is not None:
      return self._fact_history_loader(session_id)
    return self._events.load_events(session_id)

  def run_display_state(self, run: RunRecord, events: list[dict], host_boot: datetime) -> str:
    """One run's UI-facing state, derived only from facts.

    queued: registered, never launched (a durable stop request marks it
    stopped). running: launched identity still alive. attention: launched but
    nobody observed the exit. Otherwise the recorded terminal outcome.
    """
    outcome = self.terminal_outcome(events, run.id)
    if outcome is not None:
      return str(outcome)
    if run.pid is None:
      return "stopped" if self.stop_requested(events, run.id) else "queued"
    if self.run_is_active(run, events, host_boot):
      return "running"
    return "attention"

  def terminal_outcome(self, events: list[dict], run_id: str) -> str | None:
    """The run's recorded run_finished outcome, or None while it has none."""
    return terminal_outcome_in_events(events, run_id)

  async def terminal_outcome_of(self, session_id: str, run_id: str) -> str | None:
    """The run's recorded terminal outcome, read off this store by id.

    None covers both a missing run record and a run with no terminal fact
    yet; a caller that already holds the events list folds them through
    :meth:`terminal_outcome` directly.
    """
    run = await self.get_run(session_id, run_id)
    if run is None:
      return None
    return self.terminal_outcome(self.load_events_sync(session_id), run_id)

  def _fact_input_payload(self, events: list[dict], run_id: str) -> list[str]:
    """The input ids the run's recorded run_finished fact carries ([] without one)."""
    for event in events:
      if event.get("type") == ET.RUN_FINISHED and event.get("run_id") == run_id:
        payload = event.get("input_event_ids")
        return list(payload) if isinstance(payload, list) else []
    return []

  def stop_requested(self, events: list[dict], run_id: str, request_id: str | None = None) -> bool:
    """Whether a durable run_stop_requested fact exists (optionally one request_id's)."""
    return stop_requested_in_events(events, run_id, request_id)

  def run_is_active(self, run: RunRecord, events: list[dict], host_boot_time: datetime) -> bool:
    """Whether *run* holds a verified-live process (queued and dead are both false)."""
    return is_run_alive(run.pid, run.pid_start, run.started_at, host_boot_time)

  def run_has_terminal_fact(self, run: RunRecord, events: list[dict]) -> bool:
    return self.terminal_outcome(events, run.id) is not None

  def run_is_queued(self, run: RunRecord, events: list[dict]) -> bool:
    """Registered but never launched: retains its inputs for later dispatch."""
    return run.pid is None and not self.run_has_terminal_fact(run, events)

  def run_blocker(self, run: RunRecord, events: list[dict], host_boot_time: datetime) -> str | None:
    """The structural-mutation blocker one run poses, or None.

    Terminal runs block nothing; a live process is an active run; a launched
    run without a terminal fact needs recovery before structural change; a
    queued run is a pending execution request.
    """
    if self.run_has_terminal_fact(run, events):
      return None
    if run.pid is None:
      return f"run {run.id} is queued (pending dispatch)"
    if is_run_alive(run.pid, run.pid_start, run.started_at, host_boot_time):
      return f"run {run.id} is active"
    return f"run {run.id} has an unresolved process identity (needs recovery)"

  # -- registration --------------------------------------------------------

  async def register_run(self, record: RunRecord, *, task_spec_text: str | None = None) -> RunRecord:
    """Publish one run record (idempotent by id) and its compatibility thread alias.

    The pinned task-spec body lands before the metadata that references it, so
    a crash between the two leaves an unreferenced body, never a record whose
    evidence pointer dangles. Registration runs under the control lock.
    """
    async with self._lock:
      return await self.register_run_locked(record, task_spec_text=task_spec_text)

  async def register_run_locked(self, record: RunRecord, *, task_spec_text: str | None = None) -> RunRecord:
    """register_run for a caller already holding the control lock (the lock is not reentrant)."""
    existing = self.read_run_sync(record.session_id, record.id)
    if existing is not None:
      # Re-register the alias on the replay path: a crash between the original
      # metadata write and its alias write must not leave the compatibility
      # entry missing forever (the write is idempotent).
      self._aliases.register_run_thread(record.session_id, record.id)
      return existing
    run_dir = self.run_dir(record.session_id, record.id)
    if task_spec_text is not None:
      run_dir.mkdir(parents=True, exist_ok=True)
      spec_path = self.task_spec_path(record.session_id, record.id)
      atomic_write_text(spec_path, task_spec_text)
      record.task_spec_ref = str(spec_path)
      record.task_spec_hash = sha256_hex(task_spec_text)
    run_dir.mkdir(parents=True, exist_ok=True)
    path = self.metadata_path(record.session_id, record.id)
    await asyncio.to_thread(atomic_write_text, path, record.model_dump_json(indent=2))
    self._aliases.register_run_thread(record.session_id, record.id)
    # A registered Run is a new fact transition (queued work exists where none
    # did): the node's sidebar state must re-probe on the next poll. No path
    # rides the mark: a run metadata file is not a workers-panel row source,
    # and the sidebar probe's own signature walk covers the file.
    mark_sidebar_dirty(record.session_id)
    return record

  async def create_retry_run(
      self,
      session_id: str,
      request_id: str,
      original_run_id: str,
      *,
      task_spec_text: str | None = None,
      **fields: object,
  ) -> RunRecord:
    """Bind (session, request_id) to one retry run; replays return the original product."""
    async with self._lock:
      return await self.create_retry_run_locked(
          session_id, request_id, original_run_id, task_spec_text=task_spec_text, **fields)

  async def create_retry_run_locked(
      self,
      session_id: str,
      request_id: str,
      original_run_id: str,
      *,
      task_spec_text: str | None = None,
      **fields: object,
  ) -> RunRecord:
    """create_retry_run for a caller already holding the control lock."""
    run_id = stable_run_id(session_id, request_id)
    record_kwargs: dict = {"kind": "work", **fields}
    record = RunRecord(
        id=run_id,
        session_id=session_id,
        retry_of_run_id=original_run_id,
        **record_kwargs,  # type: ignore[arg-type]
    )
    return await self.register_run_locked(record, task_spec_text=task_spec_text)

  # -- launch and observation facts ----------------------------------------

  async def record_launch(self, session_id: str, run_id: str, *, pid: int, pid_start: str) -> RunRecord:
    """Persist the spawned process identity on the registered Run.

    Called from the backend's on_spawn callback the moment (pid, pid_start) is
    pinned, BEFORE any call from the run's credential can be accepted: the
    caller-identity check requires both fields on the referenced Run. The
    write takes the control lock (short metadata replace) and is idempotent —
    a repeated callback for the same process rewrites the same pair, and a
    pid_start change is a corrupted callback, not a recovery input.
    """
    async with self._lock:
      run = self._require_run(session_id, run_id)
      if run.pid is not None and run.pid_start is not None and (run.pid != pid or run.pid_start != pid_start):
        raise RunIdentityConflictError(
            f"run {run_id} already records process identity "
            f"({run.pid}, {run.pid_start!r}); refusing to overwrite with ({pid}, {pid_start!r})")
      first_launch = run.pid is None
      if run.started_at is None:
        run.started_at = utc_now()
      run.pid = pid
      run.pid_start = pid_start
      await asyncio.to_thread(atomic_write_text, self.metadata_path(session_id, run_id), run.model_dump_json(indent=2))
      # The launch fact flipped the derived state (queued -> running): the next
      # poll must re-probe (same contract as the terminal fact below). No path
      # rides the mark: a run metadata file is not a workers-panel row source.
      mark_sidebar_dirty(session_id)
      if first_launch:
        # The durable identity flipped the node's derived work state (queued ->
        # running): tell connected trees now, or a queued row painted while the
        # child process was still starting stays stale until an unrelated later
        # fact. Same best-effort seam and lock discipline as the terminal fact:
        # a notification failure is logged, never fails the durable launch, and
        # a repeated callback for the same process changes nothing.
        await self._events.notify_tree_changed(session_id, ET.RUN_LAUNCHED)
      return run

  async def record_observation(
      self,
      session_id: str,
      run_id: str,
      *,
      native_session_id: str | None = None,
      model: str | None = None,
      raw_log_ref: str | None = None,
      events_ref: str | None = None,
      result_ref: str | None = None,
      prompt_snapshot_ref: str | None = None,
      repo_path: str | None = None,
      base_branch: str | None = None,
      branch_name: str | None = None,
      worktree_path: str | None = None,
  ) -> RunRecord:
    """Persist the transport/result evidence the adapter observed on this Run.

    Called before the terminal fact lands, so the evidence exists when the
    finish does. Only provided fields are written; the terminal fact remains
    the sole owner of outcome, ended_at and exit_code.
    """
    async with self._lock:
      run = self._require_run(session_id, run_id)
      for field, value in (
          ("native_session_id", native_session_id),
          ("model", model),
          ("raw_log_ref", raw_log_ref),
          ("events_ref", events_ref),
          ("result_ref", result_ref),
          ("prompt_snapshot_ref", prompt_snapshot_ref),
          ("repo_path", repo_path),
          ("base_branch", base_branch),
          ("branch_name", branch_name),
          ("worktree_path", worktree_path),
      ):
        if value is not None:
          setattr(run, field, value)
      await asyncio.to_thread(atomic_write_text, self.metadata_path(session_id, run_id), run.model_dump_json(indent=2))
      return run

  # -- terminal facts ------------------------------------------------------

  async def record_finish(
      self,
      session_id: str,
      run_id: str,
      outcome: str,
      *,
      input_event_ids: list[str] | None = None,
      exit_code: int | None = None,
      ended_at: datetime | None = None,
  ) -> RunRecord:
    """Write the one run_finished fact (idempotent: the first terminal fact wins).

    The durable event lands before the metadata mirror (ended_at/exit_code), so
    a crash between the two leaves the fact — which every state reader
    consumes — intact.
    """
    async with self._lock:
      return await self.record_finish_locked(
          session_id, run_id, outcome, input_event_ids=input_event_ids, exit_code=exit_code, ended_at=ended_at)

  def _finish_payload(self, run: RunRecord, input_event_ids: list[str] | None) -> list[str]:
    """The durable acknowledgement payload of one finish, validated against the record.

    A dispatched run's payload is exactly its registered batch: later arrivals
    belong to the next run, and a caller can never acknowledge ids beyond the
    batch its own Run bound (a different session's or an unrelated Run's input).
    A run that never claimed a batch acknowledges only what its finisher names,
    and those ids must be input events of this same session (identity-bound,
    not arbitrary strings).
    """
    if run.input_event_ids:
      # An empty caller list is the permissive default, never a shrink: the
      # durable payload is exactly the registered batch.
      if input_event_ids and list(input_event_ids) != list(run.input_event_ids):
        raise RunInputMismatchError(
            f"run {run.id} finish payload {input_event_ids!r} does not match its registered "
            f"input batch {run.input_event_ids!r}")
      return list(run.input_event_ids)
    supplied = list(input_event_ids or [])
    if supplied:
      events = self.load_events_sync(run.session_id)
      known = {event.get("id") for event in events}
      unknown = [input_id for input_id in supplied if input_id not in known]
      if unknown:
        raise RunInputMismatchError(
            f"run {run.id} acknowledges unknown input id(s) {unknown}: not events of session "
            f"{run.session_id} and not its registered batch")
      # A batchless run acknowledges only inputs no other run already owns:
      # ids carried by another run's terminal fact are that run's inputs, and
      # ids another registered non-terminal run claimed are bound to that
      # run's batch (a stopped queued run claims nothing — it never launches).
      foreign: set[str] = set()
      for event in events:
        if event.get("type") != ET.RUN_FINISHED or event.get("run_id") == run.id:
          continue
        payload = event.get("input_event_ids")
        if isinstance(payload, list):
          foreign.update(str(i) for i in payload)
      for other in self.list_run_records_sync(run.session_id):
        if other.id == run.id or self.run_has_terminal_fact(other, events):
          continue
        if other.pid is None and self.stop_requested(events, other.id):
          continue
        foreign.update(str(i) for i in other.input_event_ids)
      stolen = [input_id for input_id in supplied if input_id in foreign]
      if stolen:
        raise RunInputMismatchError(
            f"run {run.id} cannot acknowledge input(s) {stolen}: already the "
            "acknowledged payload or claimed batch of another run")
    return supplied

  async def record_finish_locked(
      self,
      session_id: str,
      run_id: str,
      outcome: str,
      *,
      input_event_ids: list[str] | None = None,
      exit_code: int | None = None,
      ended_at: datetime | None = None,
  ) -> RunRecord:
    """record_finish for a caller already holding the control lock.

    The record is validated before anything is appended, the acknowledgement
    payload is derived from the registered batch (never a permissive caller
    default), and the durable fact lands before the metadata mirror. A repeat
    call after a crash between the two restores the mirror from the
    authoritative terminal fact without changing the first outcome.
    """
    events = self.load_events_sync(session_id)
    existing = self.terminal_outcome(events, run_id)
    run = self._require_run(session_id, run_id)
    existing_payload = self._fact_input_payload(events, run_id)
    if existing is not None:
      # Repeat reconciliation: the terminal fact is authoritative; repair the
      # metadata mirror a crash may have left behind. The first outcome and its
      # acknowledgement payload never move, whatever a repeat caller names.
      if run.input_event_ids != existing_payload or run.ended_at is None:
        run.input_event_ids = existing_payload
        run.ended_at = run.ended_at or utc_now()
        await asyncio.to_thread(
            atomic_write_text, self.metadata_path(session_id, run_id), run.model_dump_json(indent=2))
      return run
    payload = self._finish_payload(run, input_event_ids)
    event = build_control_event(
        ET.RUN_FINISHED,
        actor=ACTOR_SYSTEM,
        source_session_id=session_id,
        run_id=run_id,
        input_event_ids=payload,
        outcome=outcome,
    )
    await self._events.append(session_id, event)
    run.ended_at = ended_at or utc_now()
    run.exit_code = exit_code
    run.input_event_ids = payload
    await asyncio.to_thread(atomic_write_text, self.metadata_path(session_id, run_id), run.model_dump_json(indent=2))
    # The terminal fact flipped the derived state (running/waiting/attention ->
    # idle or the resolved verdict): the next poll must re-probe. No path rides
    # the mark: a run metadata file is not a workers-panel row source.
    mark_sidebar_dirty(session_id)
    return run

  # -- stop ----------------------------------------------------------------

  async def request_stop(self, session_id: str, run_id: str, request_id: str) -> RunStopResult:
    """One idempotent stop request: durable fact first, then identity-checked signal.

    The run_stop_requested event lands under the control lock before any
    signal is sent, so a crash right after the append leaves the request
    durably recorded for the fresh-reader recovery path. Identity validation,
    signalling, and the exit wait all run outside the lock.
    """
    async with self._lock:
      run = self._require_run(session_id, run_id)
      events = self.load_events_sync(session_id)
      existing = self.terminal_outcome(events, run_id)
      if existing is not None:
        # A naturally completed run retains its outcome; no stop request is added.
        return RunStopResult(run_id=run_id, stop_requested=False, outcome=existing)
      if not self.stop_requested(events, run_id, request_id):
        event = build_control_event(
            ET.RUN_STOP_REQUESTED,
            actor=ACTOR_SYSTEM,
            source_session_id=session_id,
            request_id=request_id,
            run_id=run_id,
        )
        await self._events.append(session_id, event)
        # The durable stop request is a fact transition (a queued run leaves the
        # waiting verdict): the next poll must re-probe.
        mark_sidebar_dirty(session_id)

    # A queued (never-launched) run has no process to signal and no exit to
    # observe; the durable request is the fact, and the dispatch stage honors
    # it instead of launching.
    if run.pid is None:
      return RunStopResult(run_id=run_id, stop_requested=True, outcome=None)
    return await self._follow_through_stop(session_id, run)

  async def reconcile_stop_request(self, session_id: str, run_id: str) -> RunStopResult:
    """Fresh-reader recovery: finish a durably requested stop from current process facts."""
    run = self._require_run(session_id, run_id)
    events = self.load_events_sync(session_id)
    existing = self.terminal_outcome(events, run_id)
    if existing is not None:
      return RunStopResult(run_id=run_id, stop_requested=False, outcome=existing)
    if not self.stop_requested(events, run_id):
      return RunStopResult(run_id=run_id, stop_requested=False, outcome=None)
    if run.pid is None:
      return RunStopResult(run_id=run_id, stop_requested=True, outcome=None)
    return await self._follow_through_stop(session_id, run)

  async def _follow_through_stop(self, session_id: str, run: RunRecord) -> RunStopResult:
    """Validate identity, signal the specific owned process, observe the exit.

    Called outside the control lock. A pid_start mismatch (pid reuse) raises
    :class:`RunIdentityConflictError` with the current /proc evidence; the
    durable stop request from the requesting phase stays on disk.
    """
    pid = run.pid
    assert pid is not None
    stat_pair = read_pid_stat(pid)
    exited = stat_pair is None or stat_pair[1] == "Z"
    if not exited:
      current_start = stat_pair[0]
      if current_start != run.pid_start:
        raise RunIdentityConflictError(
            f"run {run.id} process identity mismatch: recorded pid_start {run.pid_start!r}, "
            f"/proc/{pid} reports {current_start!r}")
      if run.started_at is None or run.started_at.tzinfo is None:
        raise RunIdentityConflictError(f"run {run.id} has no usable started_at for identity validation")
      if run.started_at.astimezone(UTC) <= read_host_boot_time():
        raise RunIdentityConflictError(
            f"run {run.id} started_at predates the current host boot; recorded identity is stale")
      try:
        os.kill(pid, signal.SIGTERM)
      except ProcessLookupError:
        # The exit beat the signal (pid vanished after the identity read): the
        # observed exit is the fact an interrupted terminal record rides on.
        exited = True
      else:
        exited = await self._wait_for_exit(pid)
    if exited:
      await self.record_finish(session_id, run.id, "interrupted")
      outcome = self.terminal_outcome(self.load_events_sync(session_id), run.id)
      return RunStopResult(run_id=run.id, stop_requested=True, outcome=outcome)
    return RunStopResult(run_id=run.id, stop_requested=True, outcome=None)

  @staticmethod
  async def _wait_for_exit(pid: int) -> bool:
    """Bounded poll for the process's actual exit after the signal."""
    deadline = time.monotonic() + STOP_EXIT_WAIT_SECONDS
    while time.monotonic() < deadline:
      stat_pair = read_pid_stat(pid)
      if stat_pair is None or stat_pair[1] == "Z":
        return True
      await asyncio.sleep(STOP_EXIT_POLL_SECONDS)
    return False
