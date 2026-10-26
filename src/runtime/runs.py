"""Run truth on disk for the restart-safe agent runtime.

A worker/master run's ground truth lives in its per-run directory, not in
the server process: the agent subprocess writes its NDJSON stdout straight to
``agent.raw.ndjson`` and stderr to ``agent.stderr.log`` via inherited file
descriptors, and the server is a tail-follow consumer that can die and
re-attach at a recorded byte offset (``agent.raw.cursor``) without the writer
noticing.

This module owns the pure/queryable parts of that contract:

- path derivation for the per-run dirs, and the on-disk file names each
  consumer joins onto the dir it already holds;
- process liveness (``pid`` + ``/proc/<pid>/stat`` field 22 + host boot time);
- the pure raw-line -> translated-event projection shared by the live read
  loop, the re-attach path, and tests;
- the end-of-run error-hint selection for a failed invocation (the
  invocation's own error events over the stderr tail);
- reading the run's true completion time (the raw log's final mtime).
"""

from __future__ import annotations

import dataclasses
import datetime
import os
import pathlib
import re
import signal
import time
from collections.abc import Awaitable, Callable, Iterable
from typing import TYPE_CHECKING

import orjson

from src.infra import event_types as ET
from src.infra import json_utils, log_once, models, ndjson
from src.runtime import chat_events, control_events, run_identity, run_token, sidebar_state

log = log_once.LazyStructlogLogger()

if TYPE_CHECKING:
  import asyncio

  from src.infra import config

RAW_LOG_NAME = "agent.raw.ndjson"
STDERR_LOG_NAME = "agent.stderr.log"
CURSOR_NAME = "agent.raw.cursor"
# The managed instructions a worker or review run hands its backend. It sits in the run directory,
# never in the worktree the run works in.
HARNESS_INSTRUCTIONS_NAME = "harness_instructions.md"

# A session's data directory, and the master-capture directory name under it: a
# task Run's transport sits in data/runs/<run_id>/ and a master run's in
# data/master_runs/<started_at>/. Writers (the master turn in src/runtime, the
# task tree's node creation) and readers (token_tally's corpus walk, the
# cold-storage transport sweep) must agree on these names.
DATA_DIR_NAME = "data"
MASTER_RUNS_DIR_NAME = "master_runs"

# The sessions-tree directory holding a session's pre-task-tree thread
# directories. No writer lays it down; the storage-cool scan,
# token_tally's corpus walk and backup's path classification still read it by
# name, so all sides must agree on this name.
THREADS_DIR_NAME = "threads"

# The chat event log inside a thread's data directory. token_tally's corpus
# walk joins the same relative suffix by string, so the names move together.
EVENTS_LOG_NAME = "events.jsonl"

# The per-session metadata filename (the task tree's node records; re-exported
# here): defined in src.runtime.run_identity, whose run-scoped CLI path reads
# session metadata without this module's model stack.
METADATA_NAME = run_identity.SESSION_METADATA_NAME


def backend_type(cfg: config.CharlieBotConfig, backend_id: str | None) -> str | None:
  """The configured transport type of ``backend_id``; None when unset or unknown."""
  if not backend_id:
    return None
  option = cfg.get_backend_option(backend_id)
  return option.type if option else None


# ---------------------------------------------------------------------------
# Process liveness
# ---------------------------------------------------------------------------


def read_host_boot_time() -> datetime.datetime:
  """Host boot time from /proc/stat ``btime`` as a tz-aware UTC datetime."""
  with open("/proc/stat", encoding="utf-8") as f:
    for line in f:
      if line.startswith("btime "):
        return datetime.datetime.fromtimestamp(int(line.split()[1]), tz=datetime.UTC)
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
    started_at: datetime.datetime | None,
    host_boot_time: datetime.datetime,
) -> bool:
  """Whether the recorded (pid, pid_start) pair still points at a live process.

  Three conjuncts pin one process instance: /proc/<pid>/stat exists and its
  field 22 equals the recorded pid_start (pid reuse cannot fake it), the
  process is not a zombie, and the run began after the host's current boot
  (nothing survives a host reboot, so a pre-boot pair is always stale).
  Descendants that inherited the raw-log fd do NOT count as liveness — the
  probe judges the recorded process alone.
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
  return started_at.astimezone(datetime.UTC) > host_boot_time


def run_alive_probe(
    pid: int | None,
    pid_start: str | None,
    started_at: datetime.datetime | None,
    host_boot_time: datetime.datetime,
) -> Callable[[], bool]:
  """The re-evaluable form of ``is_run_alive``: each call re-runs the judgment.

  kill_group_escalating re-probes between signals, so kill authorization needs
  the recorded-identity judgment as a callable, not a one-shot boolean.
  """
  return lambda: is_run_alive(pid, pid_start, started_at, host_boot_time)


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
      event = ndjson.parse_ndjson_line(piece, log_event="raw_line_not_json", log_fields={})
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
  """Terminal-status judgment from a result event."""
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


def project_raw_file(
    raw_path: pathlib.Path,
    translate: Callable[[dict], list[dict]],
    line_filter: ndjson.HeadProvableFilter | None = None) -> list[dict]:
  """Whole-file projection: read, parse, and project one raw log.

  ``translate`` must be fresh (see project_raw_events). The scan is a full
  read of the log's bytes, so event-loop callers reach it through
  asyncio.to_thread. A *line_filter* (a ndjson.HeadProvableFilter, the only filter
  whose verdict a bounded head probe can answer) bounds the parse to the
  lines its keep-set names and rides the mapped walk, so a rejected line
  costs its 256-byte probe instead of its bytes and no whole-file copy
  precedes the walk; the unfiltered scan keeps the read-bytes shape its
  existing callers measure.
  """
  if line_filter is None:
    return project_raw_events(parse_raw_lines(raw_path.read_bytes()), translate)
  return project_raw_events(
      ndjson.parse_ndjson_events(raw_path, log_event="raw_line_not_json", log_fields={}, parse_filter=line_filter),
      translate)


def scan_result_exit(
    raw_path: pathlib.Path,
    translate: Callable[[dict], list[dict]],
) -> tuple[list[dict], dict | None, int]:
  """Whole-file result scan: projected events, last result event, and its exit code.

  A missing raw log is a legal drain input (a never-started run): it scans to no
  events, no result, exit -1. The whole-file scan needs a FRESH translate — a
  stateful translate may not be reused after it consumed a stream tail. Exit code:
  0 only when a result event exists and ``result_success`` holds, else -1, the code
  a died-mid-run live turn reports for its missing result event.
  """
  events = project_raw_file(raw_path, translate) if raw_path.is_file() else []
  result = summarize_result(events)
  exit_code = 0 if result is not None and result_success(result) else -1
  return events, result, exit_code


# ---------------------------------------------------------------------------
# Completion time and cursor
# ---------------------------------------------------------------------------


def raw_completion_time(raw_path: pathlib.Path) -> datetime.datetime | None:
  """The raw log's final mtime as a tz-aware UTC datetime (None when missing).

  Backend- and content-agnostic: how long the server was down does not affect
  it, so a run that finished during downtime gets its true completion time.
  """
  try:
    st = raw_path.stat()
  except OSError:
    return None
  return datetime.datetime.fromtimestamp(st.st_mtime, tz=datetime.UTC)


def read_raw_cursor(cursor: pathlib.Path) -> int:
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

  def __init__(self, path: pathlib.Path) -> None:
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
# The run's transport event log: the executing adapter appends it and the
# transcript and history readers reopen it by name, so both sides share this
# one spelling.
RUN_EVENTS_NAME = "events.jsonl"
RUN_TASK_SPEC_NAME = "task_spec.md"

# Bounded wait after SIGTERM for the actual exit the interrupted fact requires.
STOP_EXIT_POLL_SECONDS = 0.05
STOP_EXIT_WAIT_SECONDS = 10.0
STOP_KILL_EXIT_WAIT_SECONDS = 5.0

# The caller-identity refusal pieces: defined in src.runtime.run_identity (the
# run-scoped CLI path reads them without this module's model stack); re-exported
# here so the API dependency's and the tests' established import path holds.
RUN_IDENTITY_NOT_LAUNCHED_DETAIL = run_identity.RUN_IDENTITY_NOT_LAUNCHED_DETAIL
RUN_IDENTITY_UNKNOWN_DETAIL = run_identity.RUN_IDENTITY_UNKNOWN_DETAIL
run_identity_refusal = run_identity.run_identity_refusal


@dataclasses.dataclass
class RunFacts:
  """One event list's Run facts, folded once.

  ``outcomes`` maps each run id to its last recorded run_finished outcome
  (the same last-wins rule the per-Run scans applied), and ``stop_requests``
  maps each run id to the request ids of its run_stop_requested facts.
  """
  outcomes: dict[str, str | None] = dataclasses.field(default_factory=dict)
  stop_requests: dict[str, set[str | None]] = dataclasses.field(default_factory=dict)


def fold_run_facts(facts: RunFacts, events: list[dict]) -> RunFacts:
  """Fold *events* into *facts* in place, last fact wins, and return it."""
  for event in events:
    etype = event.get("type")
    if etype not in (ET.RUN_FINISHED, ET.RUN_STOP_REQUESTED):
      continue
    run_id = event.get("run_id")
    if not isinstance(run_id, str):
      continue
    if etype == ET.RUN_FINISHED:
      facts.outcomes[run_id] = event.get("outcome")
    else:
      facts.stop_requests.setdefault(run_id, set()).add(event.get("request_id"))
  return facts


class _RunFactsMemo:
  """A bounded identity-keyed memo of one fold per event list.

  The key is the list object's identity, held under a strong reference, so a
  key hit is the same list and the covered cursor folds only the appended
  suffix; a dropped entry (or a replaced list) refolds whole. The bound keeps
  a long-lived process's dead event lists collectable.
  """

  def __init__(self, limit: int = 64) -> None:
    self._limit = limit
    self._entries: dict[int, tuple[list[dict], RunFacts, int]] = {}

  def fold(self, events: list[dict]) -> RunFacts:
    key = id(events)
    entry = self._entries.get(key)
    if entry is not None and entry[0] is events:
      facts, covered = entry[1], entry[2]
    else:
      facts, covered = RunFacts(), 0
    if covered < len(events):
      fold_run_facts(facts, events[covered:])
      covered = len(events)
    self._entries[key] = (events, facts, covered)
    while len(self._entries) > self._limit:
      self._entries.pop(next(iter(self._entries)))
    return facts


_RUN_FACTS_MEMO = _RunFactsMemo()


def terminal_outcome_in_events(events: list[dict], run_id: str) -> str | None:
  """The last recorded run_finished outcome of one Run (None while none).

  Answers from the shared one-fold-per-list memo behind every Run-fact
  question, so a caller that re-asks across a node's Runs pays one fold for
  the whole list.
  """
  return _RUN_FACTS_MEMO.fold(events).outcomes.get(run_id)


def withheld_reason_in_events(events: list[dict], run_id: str) -> str | None:
  """The newest recorded run_launch_withheld reason of one Run (None while none).

  The pure fact scan behind the queued Run's ``withheld`` display state: the
  worker transcript's header (src.runtime.worker_transcript) reads the same
  durable events through this function, so the state and its reason answer one
  identical question.
  """
  reason: str | None = None
  for event in events:
    if event.get("type") == ET.RUN_LAUNCH_WITHHELD and event.get("run_id") == run_id:
      value = event.get("reason")
      reason = value if isinstance(value, str) and value else None
  return reason


def stop_requested_in_events(
    events: list[dict],
    run_id: str,
    request_id: str | None = None,
) -> bool:
  """Whether a durable run_stop_requested fact exists (optionally one request_id's).

  Answers from the same shared fold memo as
  :func:`terminal_outcome_in_events`: one fold per event list serves every
  Run-fact question.
  """
  requests = _RUN_FACTS_MEMO.fold(events).stop_requests.get(run_id)
  if not requests:
    return False
  if request_id is None:
    return True
  return request_id in requests


class RunNotFoundError(LookupError):
  """The requested run record does not exist (API: 404)."""


# Several run-lookup paths raise this sentence inside their own miss types,
# and a route restates it as the client-visible 404 detail, so one home keeps
# the layers from drifting apart on the wording.
def run_not_found_in_task_text(run_id: str, session_id: str) -> str:
  """The task-tree run-miss sentence (API: 404 detail)."""
  return f"run {run_id} not found in task {session_id}"


def run_already_finished_text(run_id: str) -> str:
  """The refusal sentence for a Run whose terminal fact is already recorded (API: 409 detail)."""
  return f"run {run_id} already finished; it claims no new inputs"


class RunIdentityConflictError(Exception):
  """The recorded (pid, pid_start) pair no longer names one process instance (API: 409).

  The durable stop request stays on disk when this raises — it is the pending
  evidence a fresh recovery reader re-checks against the same identity rule.
  """


class RunInputMismatchError(ValueError):
  """A finish acknowledgement payload that does not match the run's registered batch."""


@dataclasses.dataclass(frozen=True)
class RunStopResult:
  """The outcome of one stop request against one run."""
  run_id: str
  stop_requested: bool
  # None while the request is only durably recorded — never a completion assertion.
  outcome: str | None


@dataclasses.dataclass(frozen=True)
class RunPageSlice:
  """One keyset page of a session's run records."""
  items: list[models.RunRecord]
  next_cursor: str | None


def _run_sort_key(run: models.RunRecord) -> tuple[datetime.datetime, str]:
  """Runs order chronologically by launch; a queued (never-launched) run sorts first."""
  started = run.started_at if run.started_at is not None else datetime.datetime.min.replace(tzinfo=datetime.UTC)
  return (started, run.id)


def _encode_run_cursor(key: tuple[datetime.datetime, str], descending: bool) -> str:
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
  return run_token.b64url_encode(orjson.dumps(payload))


def _decode_run_cursor(cursor: str) -> tuple[tuple[datetime.datetime, str], bool]:
  """Decode one run-page cursor into its boundary and page order, failing loud
  on a malformed value."""
  try:
    payload = orjson.loads(run_token.b64url_decode(cursor))
    started = datetime.datetime.fromisoformat(payload["s"]) if payload["s"] else datetime.datetime.min.replace(
        tzinfo=datetime.UTC)
    return (models.ensure_utc(started), payload["i"]), bool(payload.get("d", False))
  except (ValueError, KeyError, TypeError) as e:
    raise ValueError(f"malformed run page cursor: {cursor!r}") from e


class RunStore:
  """Owns v2 run records and every terminal fact."""

  def __init__(
      self,
      sessions_dir: pathlib.Path,
      control_lock: asyncio.Lock | None,
      events: control_events.RunEventSink | None,
  ) -> None:
    """*sessions_dir* is the store's path root; *events* None wires a read-only
    store (the CLI's run-token identity resolution): the reads fall back to the
    live chat log's parse and the write paths fail loud on the missing sink.
    *control_lock* None rides the same read-only shape — the sync reads never
    touch it, and a write path's ``async with`` fails loud on it."""
    self._sessions_dir = sessions_dir
    self._lock = control_lock  # the one short control write lock, shared with the tree owner
    self._events = events
    # Installed by the tree owner: the full fact-history reader (archived
    # segments included) the terminal/stop/identity reads use, so a rotated
    # acknowledgement or stop request never un-dones itself.
    self._fact_history_loader: Callable[[str], list[dict]] | None = None
    # Installed by the tree owner: the Run liveness notification (a worker
    # node's busy interval, the display backend) the launch/finish paths fire
    # after the durable write. Best-effort by contract: a notification failure
    # is logged and never fails the durable operation.
    self._liveness_notifier: Callable[[str, models.RunRecord, bool], Awaitable[None]] | None = None
    # Bumped after every record-mirror write below; every mutation of a record
    # a reader can observe funnels through write_record, so an unchanged
    # generation is the whole staleness contract for a reader memo keyed on it
    # (the same writer-funnel shape the sidebar fold's generation uses).
    self.records_generation = 0

  def set_fact_history_loader(self, loader: Callable[[str], list[dict]]) -> None:
    """Install the tree owner's full-history reader (archived segments included)."""
    self._fact_history_loader = loader

  def set_liveness_notifier(self, notifier: Callable[[str, models.RunRecord, bool], Awaitable[None]]) -> None:
    """Install the tree owner's Run liveness notification (launch/finish)."""
    self._liveness_notifier = notifier

  async def notify_liveness(self, session_id: str, run: models.RunRecord, *, launched: bool) -> None:
    """Fire the installed liveness notification; a failure is logged, never raised.

    The durable fact (process identity or terminal outcome) is already on
    disk when this runs and the sidebar's activity derivation reads only
    that fact, so a notification failure costs the worker's header timer
    this one transition — never the durable launch or finish.
    """
    notifier = self._liveness_notifier
    if notifier is None:
      return
    try:
      await notifier(session_id, run, launched)
    except Exception:
      log.error("run_liveness_notify_failed", session_id=session_id, run_id=run.id, launched=launched, exc_info=True)

  # -- paths ---------------------------------------------------------------

  def runs_root(self, session_id: str) -> pathlib.Path:
    return self._sessions_dir / session_id / DATA_DIR_NAME / RUNS_DIR_NAME

  def run_dir(self, session_id: str, run_id: str) -> pathlib.Path:
    return self.runs_root(session_id) / run_id

  def metadata_path(self, session_id: str, run_id: str) -> pathlib.Path:
    return self.run_dir(session_id, run_id) / RUN_METADATA_NAME

  def task_spec_path(self, session_id: str, run_id: str) -> pathlib.Path:
    return self.run_dir(session_id, run_id) / RUN_TASK_SPEC_NAME

  async def write_record(self, session_id: str, run: models.RunRecord) -> None:
    """The one record-mirror write funnel: the durable write, then the bump.

    Every mutation of a record observable through :meth:`read_run_sync` must
    ride this method, or a reader memo keyed on ``records_generation`` serves
    a stale record. The bump lands after the awaited write, on the same loop
    pass a later reader's synchronous derivation runs in.
    """
    import asyncio  # deferred: charliebot improve --help

    await asyncio.to_thread(
        json_utils.atomic_write_text, self.metadata_path(session_id, run.id), run.model_dump_json(indent=2))
    self.records_generation += 1

  # -- reads ---------------------------------------------------------------

  def read_run_sync(self, session_id: str, run_id: str) -> models.RunRecord | None:
    path = self.metadata_path(session_id, run_id)
    if not path.is_file():
      return None
    try:
      return models.RunRecord.model_validate_json(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
      raise run_identity.unreadable_run_metadata(path, e) from e

  async def get_run(self, session_id: str, run_id: str) -> models.RunRecord | None:
    import asyncio  # deferred

    return await asyncio.to_thread(self.read_run_sync, session_id, run_id)

  def _require_run(self, session_id: str, run_id: str) -> models.RunRecord:
    """The run's record, or RunNotFoundError when no run metadata exists.

    A plain read: no lock is taken here, so the under-lock write paths and
    the outside-lock recovery path share one read-or-raise.
    """
    run = self.read_run_sync(session_id, run_id)
    if run is None:
      raise RunNotFoundError(run_not_found_in_task_text(run_id, session_id))
    return run

  def list_run_records_sync(self, session_id: str) -> list[models.RunRecord]:
    """Every run record of one session, chronological (queued first)."""
    root = self.runs_root(session_id)
    if not root.is_dir():
      return []
    runs: list[models.RunRecord] = []
    for entry in root.iterdir():
      if not entry.is_dir():
        continue
      run = self.read_run_sync(session_id, entry.name)
      if run is not None:
        runs.append(run)
    runs.sort(key=_run_sort_key)
    return runs

  def newest_run_record_sync(self, session_id: str) -> models.RunRecord | None:
    """The session's newest Run record by the canonical (started_at, id) order."""
    runs = self.list_run_records_sync(session_id)
    return runs[-1] if runs else None

  def last_ended_run_id(self, session_id: str) -> str | None:
    """The id of the Run that ended last on one node, by ``ended_at`` (None before any end).

    Only a Run with a terminal fact has ended. A terminal Run whose metadata
    mirror missed its ``ended_at`` (the crash window the recovery repair
    fills) ranks by its run_finished fact's timestamp, so a pass that has
    not repaired it yet never misreads it as not-yet-ended; launch order
    breaks ties.
    """
    events = self.load_events_sync(session_id)
    fact_times: dict[str, datetime.datetime] = {}
    for event in events:
      if event.get("type") == ET.RUN_FINISHED and isinstance(event.get("run_id"), str):
        fact_times[event["run_id"]] = models.ensure_utc(datetime.datetime.fromisoformat(str(event["timestamp"])))
    last_key: tuple[datetime.datetime, int] | None = None
    last_id: str | None = None
    for position, run in enumerate(self.list_run_records_sync(session_id)):
      fact_time = fact_times.get(run.id)
      if fact_time is None:
        continue
      key = (run.ended_at or fact_time, position)
      if last_key is None or key > last_key:
        last_key, last_id = key, run.id
    return last_id

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
    if self._events is not None:
      return self._events.load_events(session_id)
    # Read-only store: the same live log the sink's session events read serves,
    # through the shared parse.
    return ndjson.parse_ndjson_file(chat_events.chat_events_path(self._sessions_dir / session_id))

  def run_display_state(self, run: models.RunRecord, events: list[dict], host_boot: datetime.datetime) -> str:
    """One run's UI-facing state, derived only from facts.

    queued: registered, never launched (a durable stop request marks it
    stopped; a recorded run_launch_withheld fact shows it as withheld with its
    reason — see withheld_reason). running: launched identity still alive.
    attention: launched but nobody observed the exit. Otherwise the recorded
    terminal outcome.
    """
    outcome = self.terminal_outcome(events, run.id)
    if outcome is not None:
      return str(outcome)
    if run.pid is None:
      if stop_requested_in_events(events, run.id):
        return "stopped"
      if self.withheld_reason(events, run.id) is not None:
        return "withheld"
      return "queued"
    if self.run_is_active(run, host_boot):
      return "running"
    return "attention"

  def terminal_outcome(self, events: list[dict], run_id: str) -> str | None:
    """The run's recorded run_finished outcome, or None while it has none."""
    return terminal_outcome_in_events(events, run_id)

  def withheld_reason(self, events: list[dict], run_id: str) -> str | None:
    """The run's recorded run_launch_withheld reason, or None while it has none."""
    return withheld_reason_in_events(events, run_id)

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

  def run_is_active(self, run: models.RunRecord, host_boot_time: datetime.datetime) -> bool:
    """Whether *run* holds a verified-live process (queued and dead are both false)."""
    return is_run_alive(run.pid, run.pid_start, run.started_at, host_boot_time)

  def run_has_terminal_fact(self, run: models.RunRecord, events: list[dict]) -> bool:
    return terminal_outcome_in_events(events, run.id) is not None

  def run_blocker(self, run: models.RunRecord, events: list[dict], host_boot_time: datetime.datetime) -> str | None:
    """The structural-mutation blocker one run poses, or None.

    Terminal runs block nothing; a live process is an active run; a launched
    run without a terminal fact needs recovery before structural change; a
    queued run is a pending execution request — except one with a durable
    stop request, which never launched and is settled by that request (the
    same verdict the work-state fold and the dispatch stage already read).
    """
    if self.run_has_terminal_fact(run, events):
      return None
    if run.pid is None:
      if stop_requested_in_events(events, run.id):
        return None  # a stopped queued run is resolved-by-request, not pending work
      return f"run {run.id} is queued (pending dispatch)"
    if is_run_alive(run.pid, run.pid_start, run.started_at, host_boot_time):
      return f"run {run.id} is active"
    return f"run {run.id} has an unresolved process identity (needs recovery)"

  # -- registration --------------------------------------------------------

  async def register_run_locked(
      self, record: models.RunRecord, *, task_spec_text: str | None = None) -> models.RunRecord:
    """Publish one run record (idempotent by id); the caller holds the control lock (the lock is not reentrant).

    The pinned task-spec body lands before the metadata that references it, so
    a crash between the two leaves an unreferenced body, never a record whose
    evidence pointer dangles.
    """
    existing = self.read_run_sync(record.session_id, record.id)
    if existing is not None:
      return existing
    run_dir = self.run_dir(record.session_id, record.id)
    if task_spec_text is not None:
      run_dir.mkdir(parents=True, exist_ok=True)
      spec_path = self.task_spec_path(record.session_id, record.id)
      json_utils.atomic_write_text(spec_path, task_spec_text)
      record.task_spec_ref = str(spec_path)
      record.task_spec_hash = control_events.sha256_hex(task_spec_text)
    run_dir.mkdir(parents=True, exist_ok=True)
    await self.write_record(record.session_id, record)
    # A registered Run is a new fact transition (queued work exists where none
    # did): the node's sidebar state must re-probe on the next poll. No path
    # rides the mark: the sidebar probe's own signature walk covers the file.
    sidebar_state.mark_sidebar_dirty(record.session_id)
    return record

  async def create_retry_run_locked(
      self,
      session_id: str,
      request_id: str,
      original_run_id: str,
      *,
      task_spec_text: str | None,
      **fields: object,
  ) -> models.RunRecord:
    """Bind (session, request_id) to one retry run; replays return the original product.

    The caller holds the control lock (the lock is not reentrant).
    """
    run_id = control_events.stable_run_id(session_id, request_id)
    record_kwargs: dict = {"kind": "work", **fields}
    record = models.RunRecord(
        id=run_id,
        session_id=session_id,
        retry_of_run_id=original_run_id,
        **record_kwargs,  # type: ignore[arg-type]
    )
    return await self.register_run_locked(record, task_spec_text=task_spec_text)

  # -- launch and observation facts ----------------------------------------

  async def record_launch(self, session_id: str, run_id: str, *, pid: int, pid_start: str) -> models.RunRecord:
    """Persist the identity of the Run's newest process on the registered Run.

    Called from the backend's on_spawn callback the moment (pid, pid_start) is
    pinned, BEFORE any call from the run's credential can be accepted: the
    caller-identity check requires both fields on the referenced Run. The
    identity is the newest process of the Run: an account relay replaces the
    previous process's pair with its own, while a Run with a stop request
    takes no new process — the write is refused and the record stays as it
    is. The write takes the control lock (short metadata replace) and is
    idempotent — a repeated callback for the same process rewrites the same
    pair.
    """
    async with self._lock:
      run = self._require_run(session_id, run_id)
      if self.stop_requested(self.load_events_sync(session_id), run_id):
        raise RuntimeError(f"run {run_id} has a stop request; refusing new process ({pid}, {pid_start!r})")
      first_launch = run.pid is None
      if run.started_at is None:
        run.started_at = models.utc_now()
      run.pid = pid
      run.pid_start = pid_start
      await self.write_record(session_id, run)
      # The launch fact flipped the derived state (queued -> running): the next
      # poll must re-probe (same contract as the terminal fact below). No path
      # rides the mark: the sidebar probe's own signature walk covers the file.
      sidebar_state.mark_sidebar_dirty(session_id)
      if first_launch:
        # The durable identity flipped the node's derived work state (queued ->
        # running): tell connected trees now, or a queued row painted while the
        # child process was still starting stays stale until an unrelated later
        # fact. Same best-effort seam and lock discipline as the terminal fact:
        # a notification failure is logged, never fails the durable launch, and
        # a repeated callback for the same process changes nothing.
        await self._events.notify_tree_changed(session_id, ET.RUN_LAUNCHED)
        await self.notify_liveness(session_id, run, launched=True)
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
  ) -> models.RunRecord:
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
      await self.write_record(session_id, run)
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
      ended_at: datetime.datetime | None = None,
  ) -> models.RunRecord:
    """Write the one run_finished fact (idempotent: the first terminal fact wins).

    The durable event lands before the metadata mirror (ended_at/exit_code), so
    a crash between the two leaves the fact — which every state reader
    consumes — intact.
    """
    async with self._lock:
      run = await self.record_finish_locked(
          session_id, run_id, outcome, input_event_ids=input_event_ids, exit_code=exit_code, ended_at=ended_at)
    await self.notify_liveness(session_id, run, launched=False)
    return run

  def _finish_payload(self, run: models.RunRecord, input_event_ids: list[str] | None) -> list[str]:
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
      ended_at: datetime.datetime | None = None,
  ) -> models.RunRecord:
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
      # metadata mirror a crash (or an out-of-space write) may have left
      # behind. The first outcome and its acknowledgement payload never move,
      # whatever a repeat caller names. An empty ended_at is the half-written
      # end record: the caller's ended_at/exit_code (the drain rule's values)
      # fill the empty fields only — values already written stay unchanged.
      if run.input_event_ids != existing_payload or run.ended_at is None:
        run.input_event_ids = existing_payload
        if run.ended_at is None:
          run.ended_at = ended_at or models.utc_now()
          if exit_code is not None:
            run.exit_code = exit_code
        await self.write_record(session_id, run)
      return run
    payload = self._finish_payload(run, input_event_ids)
    if self.stop_requested(events, run_id):
      outcome = "interrupted"
    event = control_events.build_control_event(
        ET.RUN_FINISHED,
        actor=control_events.ACTOR_SYSTEM,
        source_session_id=session_id,
        run_id=run_id,
        input_event_ids=payload,
        outcome=outcome,
    )
    await self._events.append(session_id, event)
    run.ended_at = ended_at or models.utc_now()
    run.exit_code = exit_code
    run.input_event_ids = payload
    await self.write_record(session_id, run)
    # The terminal fact flipped the derived state (running/waiting -> idle):
    # the next poll must re-probe. No path rides
    # the mark: the sidebar probe's own signature walk covers the file.
    sidebar_state.mark_sidebar_dirty(session_id)
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
      await self.append_stop_request_locked(session_id, run_id, request_id)

    # A queued (never-launched) run has no process to signal and no exit to
    # observe; the durable request is the fact, and the dispatch stage honors
    # it instead of launching.
    if run.pid is None:
      return RunStopResult(run_id=run_id, stop_requested=True, outcome=None)
    return await self._follow_through_stop(session_id, run)

  async def append_stop_request_locked(self, session_id: str, run_id: str, request_id: str) -> bool:
    """Append one stop fact while the caller holds the shared control lock.

    Returns True when this request id already exists or was appended. A
    terminal Run cannot acquire a new stop request.
    """
    events = self.load_events_sync(session_id)
    if self.terminal_outcome(events, run_id) is not None:
      return False
    if self.stop_requested(events, run_id, request_id):
      return True
    event = control_events.build_control_event(
        ET.RUN_STOP_REQUESTED,
        actor=control_events.ACTOR_SYSTEM,
        source_session_id=session_id,
        request_id=request_id,
        run_id=run_id,
    )
    await self._events.append(session_id, event)
    sidebar_state.mark_sidebar_dirty(session_id)
    return True

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

  async def _follow_through_stop(self, session_id: str, run: models.RunRecord) -> RunStopResult:
    """Validate identity, signal the specific owned process, observe the exit.

    Called outside the control lock. A pid_start mismatch (pid reuse) raises
    :class:`RunIdentityConflictError` with the current /proc evidence; the
    durable stop request from the requesting phase stays on disk.
    """
    pid = run.pid
    assert pid is not None
    exited = not self._validate_stop_identity(run)
    if not exited:
      try:
        os.killpg(pid, signal.SIGTERM)
      except ProcessLookupError:
        # Re-read the identity because the process can leave its group after
        # the group-id check without exiting.
        exited = not self._validate_stop_identity(run)
      else:
        exited = await self._wait_for_exit(pid, STOP_EXIT_WAIT_SECONDS)
    if not exited:
      exited = not self._validate_stop_identity(run)
      if not exited:
        try:
          os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
          exited = not self._validate_stop_identity(run)
        else:
          exited = await self._wait_for_exit(pid, STOP_KILL_EXIT_WAIT_SECONDS)
    if exited:
      await self.record_finish(session_id, run.id, "interrupted")
      outcome = self.terminal_outcome(self.load_events_sync(session_id), run.id)
      return RunStopResult(run_id=run.id, stop_requested=True, outcome=outcome)
    return RunStopResult(run_id=run.id, stop_requested=True, outcome=None)

  @staticmethod
  def _validate_stop_identity(run: models.RunRecord) -> bool:
    """Return whether the recorded process is live and safe to signal."""
    pid = run.pid
    assert pid is not None
    stat_pair = read_pid_stat(pid)
    if stat_pair is None or stat_pair[1] == "Z":
      return False
    current_start = stat_pair[0]
    if current_start != run.pid_start:
      raise RunIdentityConflictError(
          f"run {run.id} process identity mismatch: recorded pid_start {run.pid_start!r}, "
          f"/proc/{pid} reports {current_start!r}")
    if run.started_at is None or run.started_at.tzinfo is None:
      raise RunIdentityConflictError(f"run {run.id} has no usable started_at for identity validation")
    if run.started_at.astimezone(datetime.UTC) <= read_host_boot_time():
      raise RunIdentityConflictError(
          f"run {run.id} started_at predates the current host boot; recorded identity is stale")
    try:
      process_group_id = os.getpgid(pid)
    except ProcessLookupError:
      return False
    if process_group_id != pid:
      raise RunIdentityConflictError(
          f"run {run.id} process group mismatch: run pid {pid}, process group id {process_group_id}")
    return True

  @staticmethod
  async def _wait_for_exit(pid: int, timeout: float) -> bool:
    """Bounded poll for the process's actual exit after the signal."""
    import asyncio  # deferred

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
      stat_pair = read_pid_stat(pid)
      if stat_pair is None or stat_pair[1] == "Z":
        return True
      await asyncio.sleep(STOP_EXIT_POLL_SECONDS)
    return False
