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
- the outcome table (six rows) mapping on-disk facts to a ``RunOutcome``;
- the pure raw-line -> translated-event projection shared by the live read
  loop, the re-attach path, and tests;
- reading the run's true completion time (the raw log's final mtime).
"""

import json
import os
import stat
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path

import structlog

from src.core import event_types as ET
from src.core.timeouts import NO_OUTPUT_REPORT_THRESHOLD

log = structlog.get_logger()

RAW_LOG_NAME = "agent.raw.ndjson"
STDERR_LOG_NAME = "agent.stderr.log"
CURSOR_NAME = "agent.raw.cursor"

# Backend types whose event transport does not go through the shared base read
# loop (opencode serves events over its own HTTP SSE; antigravity and tui-cli
# manage their own pipes). A restart cannot attach to those, so an interrupted
# run on one of them still fails — but with this explicit reason, never
# disguised as a crash.
UNCOVERED_BACKEND_TYPES = frozenset({"opencode", "antigravity", "tui-cli"})
TRANSPORT_NOT_COVERED_REASON = "backend transport not covered by restart-safe runtime"

LEGACY_RAW_MISSING_REASON = "raw log missing (run predates restart-safe transport)"
DIED_WITHOUT_RESULT_REASON = "process exited without a final result event"

# Effective-alive verdicts: wherever death cannot be PROVEN (a liveness input
# is missing, or the probe says alive), the run is treated as alive and never
# finalized failed on missing evidence. These reasons route init.py's
# report-only branch (no re-attach) for rows that have nothing followable.
UNCOVERED_ALIVE_REASON = "uncovered-alive"
RAW_MISSING_ALIVE_REASON = "raw-missing-alive"

# Improve-loop iteration threads are identified by their description prefix;
# the loop task itself does not survive a restart (loop continuation is an
# explicit non-goal), so these threads are finalized, never respawned, and
# the shutdown path terminates their processes along with the loop.
IMPROVE_ITERATION_PREFIX = "Iterative improvement — iteration"

# Pids that must never appear in a kill list derived from the fd scan.
_NEVER_KILL_PIDS = frozenset({0, 1})


def backend_type(cfg, backend_id: str | None) -> str | None:
  """The configured transport type of ``backend_id``; None when unset or unknown."""
  if not backend_id:
    return None
  option = cfg.get_backend_option(backend_id)
  return option.type if option else None


class RunOutcome(str, Enum):
  """The six outcome-table rows for an interrupted run (plan r11 section 4.1)."""
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
  # Descendants that outlived the run while holding its raw-log fd (row 5).
  leftover_holders: tuple[HolderProcess, ...] = ()


# ---------------------------------------------------------------------------
# Path derivation
# ---------------------------------------------------------------------------


def raw_log_path(thread_dir: Path) -> Path:
  return thread_dir / "data" / RAW_LOG_NAME


def stderr_log_path(thread_dir: Path) -> Path:
  return thread_dir / "data" / STDERR_LOG_NAME


def cursor_path(thread_dir: Path) -> Path:
  return thread_dir / "data" / CURSOR_NAME


# ---------------------------------------------------------------------------
# Process liveness
# ---------------------------------------------------------------------------


def read_host_boot_time() -> datetime:
  """Host boot time from /proc/stat ``btime`` as a tz-aware UTC datetime."""
  with open("/proc/stat", "r", encoding="utf-8") as f:
    for line in f:
      if line.startswith("btime "):
        return datetime.fromtimestamp(int(line.split()[1]), tz=timezone.utc)
  raise RuntimeError("/proc/stat missing btime line")


def read_pid_stat(pid: int) -> tuple[str, str] | None:
  """Return (start_time_field, state) from /proc/<pid>/stat, or None when gone.

  Field 22 (process start time) is a u64 in kernel clock ticks; keep it as the
  raw string so comparisons are exact and overflow-free. The comm field may
  contain spaces and closing parens, so fields are split after the LAST ')':
  field 3 (state) is then index 0 and field 22 is index 19.
  """
  try:
    with open(f"/proc/{pid}/stat", "r", encoding="utf-8") as f:
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
  return started_at.astimezone(timezone.utc) > host_boot_time


# ---------------------------------------------------------------------------
# Descendant discovery (diagnostic only — never a liveness input)
# ---------------------------------------------------------------------------


def scan_stdout_holders() -> dict[tuple[int, int], list[HolderProcess]]:
  """Map (st_dev, st_ino) -> processes whose fd 1 points at that regular file.

  One ``/proc/*/fd/1`` scan (~3 ms for ~500 processes). fd 1 follows the
  symlink to whatever the process's stdout is; only regular files are indexed
  (a run's raw log; consoles/pipes/sockets are skipped). Used exclusively to
  find descendants that outlived their run's process group — it feeds reporting
  and row-5 cleanup, never liveness judgments.
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
    if holder.pid == run_pid or holder.pid == own or holder.pid in _NEVER_KILL_PIDS:
      continue
    out.append(holder)
  return tuple(out)


# ---------------------------------------------------------------------------
# Raw -> event projection (pure)
# ---------------------------------------------------------------------------


def parse_raw_lines(raw_bytes: bytes) -> list[dict]:
  """Decode+parse raw log bytes into event dicts, skipping blank/torn lines.

  A trailing partial line (written by a process killed mid-write) is dropped —
  its offset stays un-consumed semantics make re-reading it produce at most a
  duplicate, never a loss.
  """
  events: list[dict] = []
  for raw_line in raw_bytes.split(b"\n"):
    line = raw_line.decode("utf-8", errors="replace").strip()
    if not line:
      continue
    try:
      events.append(json.loads(line))
    except json.JSONDecodeError as e:
      log.debug("raw_line_not_json", error=str(e))
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
  return datetime.fromtimestamp(st.st_mtime, tz=timezone.utc)


def read_raw_cursor(cursor: Path) -> int:
  """Consumed byte offset; 0 (replay) on a missing or unparseable cursor file."""
  try:
    return int(cursor.read_text(encoding="utf-8").strip())
  except (OSError, ValueError):
    return 0


def write_raw_cursor(cursor: Path, offset: int) -> None:
  cursor.parent.mkdir(parents=True, exist_ok=True)
  cursor.write_text(str(offset), encoding="utf-8")


# ---------------------------------------------------------------------------
# Outcome resolution (the six-row table)
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
    now: datetime | None = None,
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
  a whole reconcile pass; when given and the run is not alive, row-5 leftover
  descendants are attached to the resolution (the outcome itself still comes
  from the other rows).
  """
  now = now or datetime.now(timezone.utc)
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
    events = project_raw_events(parse_raw_lines(raw_path.read_bytes()), translate)
    result = summarize_result(events)

  if backend_type in UNCOVERED_BACKEND_TYPES and result is None:
    # A result event already on disk falls through to the downstream result
    # row and completes normally; this row only judges runs without one.
    if effectively_alive:
      return RunResolution(outcome=RunOutcome.RUNNING, reason=UNCOVERED_ALIVE_REASON, completed_at=completed_at)
    return RunResolution(
        outcome=RunOutcome.DIED, reason=TRANSPORT_NOT_COVERED_REASON, completed_at=completed_at)
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
      reason = (
          f"alive but no raw output for {int(silent_for)}s "
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
