"""The worker node's main-chat transcript: its Runs' events in Run order.

A v2 worker node has no conversation of its own — its record is the set of Run
event logs under ``data/runs/``. This module projects those logs into the same
event-list shape the main chat reads for any session, so the existing message
aggregator, pagination, and turn folds render a worker node exactly like a
chat: each Run opens with one synthesized header line naming its kind, backend
and state, and a resolved task closes with a delivery summary carrying the four
evidence links. Nothing here writes to disk; every event is derived.

The projection is memoized behind a stat-only signature (run metadata and
events files plus the node's chat facts), so a repeat read of an unchanged
transcript costs one scandir and a few stats. Legacy worker threads (a
pre-task-tree delegation living in the parent session's ``threads/`` directory)
project through :func:`load_thread_transcript` — same event shape, addressed by
the parent session id plus the thread id (URL ``/?session=<parent>&thread=<id>``).

Event ordinals are positions in the synthesized list, so the bootstrap tail,
the ``/events`` pagination pages, and the transcript poll all speak one cursor
space per node.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

from src.infra import event_types as ET
from src.infra.memo import BoundedMemo, stat_signature
from src.infra.models import RunRecord, ThreadMetadata, utc_now_iso
from src.runtime.message_projection import MessageProjection
from src.runtime.runs import RUN_EVENTS_NAME, RUN_METADATA_NAME
from src.runtime.task_prompts import LAUNCH_TEXT_FILENAME
from src.runtime.threads import METADATA_NAME, THREADS_DIR_NAME, thread_events_log_path

# The projection memos: session (or session+thread) -> entry. Bounded like the
# other read-path memos; one open worker page holds one entry.
_WORKER_PROJECTION_MEMO_LIMIT = 32
_THREAD_PROJECTION_MEMO_LIMIT = 32

_worker_memo: BoundedMemo[str, TranscriptEntry] = BoundedMemo(_WORKER_PROJECTION_MEMO_LIMIT)
_thread_memo: BoundedMemo[str, TranscriptEntry] = BoundedMemo(_THREAD_PROJECTION_MEMO_LIMIT)

# Run events whose only role is process bookkeeping, never a chat row: the
# native-session adoption signal opens a run interval for the stable-prefix
# scanner, which a worker transcript never closes, so it is filtered before
# the fold — the same skip the legacy thread events endpoint applies.
_TRANSCRIPT_SKIP_TYPES = frozenset({ET.SESSION_ATTACHED})

# The states a stop control can act on: a live process, or a queued
# reservation whose durable stop request the dispatch stage honors.
_SIGNALLABLE_STATES = frozenset({"running", "queued"})

# The states the header reads as a failure (the retired leaf card folded all
# three into ``failed``): the header then carries the Run's own error text.
_FAILED_STATES = frozenset({"failed", "interrupted", "attention"})


@dataclass
class TranscriptEntry:
  """One memoized transcript projection plus the facts its readers need."""

  revision: str
  events: list[dict]
  projection: MessageProjection
  active_run_id: str | None = None
  task_state: str = "open"
  states: dict[str, str] = field(default_factory=dict)


def _backend_label(cfg, backend: str | None) -> str:
  """The configured display label for *backend*, or the raw id."""
  if not backend:
    return ""
  option = cfg.get_backend_option(backend)
  return option.label if option is not None else backend


def _run_header_event(
    *,
    run_id: str,
    kind: str,
    backend: str | None,
    backend_label: str,
    state: str,
    error: str,
    started_at: datetime | None,
    terminal_at: datetime | None,
    task_spec_ref: str,
    launch_prompt_ref: str,
    withheld: str | None = None,
) -> dict:
  """The one header line that opens a transcript segment: a Run's, or a legacy thread's.

  Both producers build the event here because one fold consumes both:
  message_aggregator's ``ET.RUN_HEADER`` case renders either the same way. The
  header carries one real time, from the projected record's own facts and
  never a clock read: the start while it ran, the terminal fact's time (a
  Run's ended_at, a thread's completed_at) when it never started, else no
  time. A page-load or projection-build time would show a dead record a
  future-lying bubble (and once fed a negative duration).

  ``state`` stays the record's display state for every existing reader (the
  sidebar, the stop control, the dot color); ``launch_failed`` only renames
  the content word (message_aggregator) for a Run that failed before its agent
  process started — no started_at, no launch prompt ever written. The two refs
  feed the header's link row (gray placeholders when empty); a legacy thread
  passes both empty. ``withheld`` is a withheld Run's reason (the durable
  run_launch_withheld fact's own words); the content word renders it as
  "withheld · <reason>".
  """
  started = started_at.isoformat() if started_at is not None else None
  terminal = terminal_at.isoformat() if terminal_at is not None else None
  return {
      "type": ET.RUN_HEADER,
      "run_id": run_id,
      "kind": kind,
      "backend": backend or "",
      "backend_label": backend_label,
      "state": state,
      "error": error,
      "withheld": withheld,
      "started_at": started,
      "timestamp": started or terminal,
      "launched": started_at is not None,
      "launch_failed": state in _FAILED_STATES and started_at is None,
      "task_spec_ref": task_spec_ref,
      "launch_prompt_ref": launch_prompt_ref,
  }


def _header_error(state: str, events: list[dict]) -> tuple[str, int | None]:
  """A failed Run's newest non-empty error-event text and its list position.

  The same walk the parent failure report reads
  (review._worker_error_from_events_log): a Run that died before its process
  started carries its actual error as its events log's error event, and the
  header shows it in full where the event's own chat line truncates. The
  position is the index into *events* of the event the text came from, so the
  projection can drop that one line from the Run's segment — the header
  already carries the text in full, and the truncated chat row would read it
  twice. ("", None) when the state is not a failure or no error event speaks.
  """
  if state not in _FAILED_STATES:
    return "", None
  for index in range(len(events) - 1, -1, -1):
    event = events[index]
    if event.get("type") != ET.ERROR:
      continue
    for key in ("message", "content"):
      value = event.get(key)
      if isinstance(value, str) and value.strip():
        return value.strip(), index
  return "", None


def _delivery_event(task_state: str, facts_events: list[dict], runs: list[RunRecord]) -> dict | None:
  """The closing delivery summary once the task stands resolved (not open).

  The summary and result refs ride the node's task_closed fact; the four
  evidence links ride the delivered Run's record (newest successful run, else
  the newest run when a resolved task never recorded a success).
  """
  if task_state == "open" or not runs:
    return None
  summary = ""
  close_refs: list[str] = []
  outcomes: dict[str, str] = {}
  for event in facts_events:
    kind = event.get("type")
    if kind == ET.TASK_CLOSED:
      summary = str(event.get("summary") or "")
      close_refs = [str(ref) for ref in (event.get("result_refs") or [])]
    elif kind == ET.RUN_FINISHED:
      outcomes[str(event.get("run_id"))] = str(event.get("outcome") or "")
  delivered = next((run for run in reversed(runs) if outcomes.get(run.id) == "success"), runs[-1])
  return {
      "type": ET.RUN_DELIVERY,
      "task_state": task_state,
      "summary": summary,
      "result_refs": close_refs,
      "raw_log_ref": delivered.raw_log_ref or "",
      "events_ref": delivered.events_ref or "",
      "result_ref": delivered.result_ref or "",
      "repo_path": delivered.repo_path or "",
      "base_branch": delivered.base_branch or "",
      "branch_name": delivered.branch_name or "",
      "run_id": delivered.id,
      "timestamp": utc_now_iso(),
  }


def _revision(signature: tuple, states: dict[str, str], task_state: str) -> str:
  """The client-facing revision: moves exactly when a re-render (reset) is due."""
  payload = json.dumps(
      [[*item] if isinstance(item, tuple) else item for item in signature] + [sorted(states.items()), task_state],
      sort_keys=True,
      default=str)
  return hashlib.sha256(payload.encode()).hexdigest()[:24]


def _read_events(path: Path) -> list[dict]:
  """One events.jsonl parsed; a missing or unreadable log is an empty segment."""
  from src.runtime.runs import parse_raw_lines
  try:
    raw = path.read_bytes()
  except OSError:
    return []
  if not raw:
    return []
  return [event for event in parse_raw_lines(raw) if event.get("type") not in _TRANSCRIPT_SKIP_TYPES]


# ---------------------------------------------------------------------------
# Worker node (v2): data/runs/<run_id>/events.jsonl per Run
# ---------------------------------------------------------------------------


def worker_signature_sync(tree, session_id: str) -> tuple:
  """Stat-only identity of every file the worker transcript reads."""
  root = tree.runs.runs_root(session_id)
  parts: list[tuple] = []
  if root.is_dir():
    for entry in sorted(root.iterdir(), key=lambda e: e.name):
      if not entry.is_dir():
        continue
      parts.append((entry.name, stat_signature(entry / RUN_METADATA_NAME), stat_signature(entry / RUN_EVENTS_NAME)))
  parts.append(("chat", stat_signature(tree.sessions.get_chat_events_path(session_id))))
  return tuple(parts)


def build_worker_transcript_sync(tree, session_id: str) -> TranscriptEntry:
  """Build the worker transcript: one header per Run, its events, the close.

  The caller (API layer) has already established that *session_id* is a
  task-tree worker node; this builder reads only the run store and the node's
  chat facts.
  """
  runs = tree.runs.list_run_records_sync(session_id)
  facts_events = tree.runs.load_events_sync(session_id)
  from src.runtime.runs import read_host_boot_time
  host_boot = read_host_boot_time()
  states = {run.id: tree.runs.run_display_state(run, facts_events, host_boot) for run in runs}
  transcript: list[dict] = []
  for run in runs:
    state = states.get(run.id, "queued")
    run_dir = tree.runs.run_dir(session_id, run.id)
    run_events = _read_events(run_dir / RUN_EVENTS_NAME)
    error, error_index = _header_error(state, run_events)
    if error_index is not None:
      # The header carries this event's text in full; the chat row would read
      # it twice. The drop lives in this projection only — events.jsonl keeps
      # every line.
      run_events = run_events[:error_index] + run_events[error_index + 1:]
    launch_prompt_path = run_dir / LAUNCH_TEXT_FILENAME
    transcript.append(
        _run_header_event(
            run_id=run.id,
            kind=run.kind,
            backend=run.backend,
            backend_label=_backend_label(tree.cfg, run.backend),
            state=state,
            error=error,
            started_at=run.started_at,
            terminal_at=run.ended_at,
            task_spec_ref=run.task_spec_ref or "",
            # The launch prompt's presence is judged by the file (Runs older
            # than launch_prompt.md started without one), never by started_at.
            launch_prompt_ref=str(launch_prompt_path) if launch_prompt_path.is_file() else "",
            withheld=(tree.runs.withheld_reason(facts_events, run.id) if state == "withheld" else None),
        ))
    transcript.extend(run_events)
  task_state = tree.task_state(session_id)
  delivery = _delivery_event(task_state, facts_events, runs)
  if delivery is not None:
    transcript.append(delivery)
  active_run_id = next((run.id for run in reversed(runs) if states.get(run.id) in _SIGNALLABLE_STATES), None)
  signature = worker_signature_sync(tree, session_id)
  return TranscriptEntry(
      revision=_revision(signature, states, task_state),
      events=transcript,
      projection=MessageProjection(list(transcript)),
      active_run_id=active_run_id,
      task_state=task_state,
      states=states)


def load_worker_transcript(tree, session_id: str) -> TranscriptEntry:
  """The memoized worker transcript; a moved signature or state rebuilds.

  The revision folds the CURRENT task state, not the cached one — a task
  close changes only that input, and a memo check over the cached value would
  never observe it (the delivered close would wait for an unrelated run-file
  move to surface). The facts fold is memoized, so the read is in-memory.
  """
  signature = worker_signature_sync(tree, session_id)
  task_state = tree.task_state(session_id)
  cached = _worker_memo.get(session_id)
  if cached is not None and cached.revision == _revision(signature, cached.states, task_state):
    return cached
  entry = build_worker_transcript_sync(tree, session_id)
  _worker_memo.store(session_id, entry)
  return entry


# ---------------------------------------------------------------------------
# Legacy worker thread: threads/<thread_id>/data/events.jsonl
# ---------------------------------------------------------------------------


def _thread_dir(session_dir: Path, thread_id: str) -> Path:
  return session_dir / THREADS_DIR_NAME / thread_id


def thread_signature_sync(session_dir: Path, thread_id: str) -> tuple:
  """Stat-only identity of one legacy thread's metadata and events files."""
  return (
      stat_signature(_thread_dir(session_dir, thread_id) / METADATA_NAME),
      stat_signature(thread_events_log_path(session_dir, thread_id)))


def _thread_state(meta: ThreadMetadata) -> str:
  """The thread's display state: its recorded status, liveness-checked while running."""
  if meta.status != "running":
    return str(meta.status)
  from src.runtime.runs import is_run_alive, read_host_boot_time
  alive = (
      meta.pid is not None and meta.pid_start is not None and meta.started_at is not None and
      is_run_alive(meta.pid, meta.pid_start, meta.started_at, read_host_boot_time()))
  return "running" if alive else "attention"


def build_thread_transcript_sync(
    meta: ThreadMetadata,
    events_path: Path,
    session_dir: Path,
    label: str,
) -> TranscriptEntry:
  """Build one legacy thread's transcript: one header line, then its events."""
  state = _thread_state(meta)
  thread_events = _read_events(events_path)
  error, error_index = _header_error(state, thread_events)
  if error_index is not None:
    # The same once-only error read the Run headers follow: the header carries
    # the text in full, so the event's own chat line leaves the projection.
    thread_events = thread_events[:error_index] + thread_events[error_index + 1:]
  transcript: list[dict] = [
      _run_header_event(
          run_id=meta.id,
          kind="thread",
          backend=meta.backend,
          backend_label=label,
          state=state,
          error=error,
          started_at=meta.started_at,
          terminal_at=meta.completed_at,
          # Legacy threads kept no per-thread task file: both refs stay empty
          # and the header's link row renders its gray placeholders.
          task_spec_ref="",
          launch_prompt_ref="",
      )
  ]
  transcript.extend(thread_events)
  signature = thread_signature_sync(session_dir, meta.id)
  return TranscriptEntry(
      revision=_revision(signature, {"thread": state}, state),
      events=transcript,
      projection=MessageProjection(list(transcript)),
      active_run_id=meta.id if state == "running" else None,
      task_state="open",
      states={"thread": state})


def load_thread_transcript(
    cfg,
    session_dir: Path,
    meta: ThreadMetadata,
    events_path: Path,
) -> TranscriptEntry:
  """The memoized legacy-thread transcript, addressed by (parent session, thread).

  The caller resolved the thread metadata (the async ThreadManager read) and
  passes its config and paths in; the fold and file reads stay in the caller's
  executor thread.
  """
  memo_key = f"{meta.session_id}:{meta.id}"
  state = _thread_state(meta)
  signature = thread_signature_sync(session_dir, meta.id)
  cached = _thread_memo.get(memo_key)
  if cached is not None and cached.revision == _revision(signature, {"thread": state}, state):
    return cached
  entry = build_thread_transcript_sync(meta, events_path, session_dir, _backend_label(cfg, meta.backend))
  _thread_memo.store(memo_key, entry)
  return entry


def thread_thinking_since(meta: ThreadMetadata) -> datetime | None:
  """The thread view's running-timer anchor: the thread's start while it runs."""
  return meta.started_at if _thread_state(meta) == "running" else None
