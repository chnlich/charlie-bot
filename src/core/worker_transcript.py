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
import os
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

from src.core import event_types as ET
from src.core.memo import BoundedMemo
from src.core.message_projection import MessageProjection
from src.core.models import RunRecord, ThreadMetadata, utc_now

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


def _run_header_event(run: RunRecord, state: str, label: str, error: str) -> dict:
  """The one header line that opens *run*'s transcript segment."""
  started = run.started_at.isoformat() if run.started_at is not None else None
  return {
      "type": ET.RUN_HEADER,
      "run_id": run.id,
      "kind": run.kind,
      "backend": run.backend or "",
      "backend_label": label,
      "state": state,
      "error": error,
      "started_at": started,
      "timestamp": started or utc_now().isoformat(),
  }


def _header_error(state: str, events: list[dict]) -> str:
  """A failed Run's newest non-empty error-event text, else "".

  The same walk the parent failure report reads
  (review._worker_error_from_events_log): a Run that died before its process
  started carries its actual error as its events log's error event, and the
  header shows it in full where the event's own chat line truncates.
  """
  if state not in _FAILED_STATES:
    return ""
  for event in reversed(events):
    if event.get("type") != ET.ERROR:
      continue
    for key in ("message", "content"):
      value = event.get(key)
      if isinstance(value, str) and value.strip():
        return value.strip()
  return ""


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
      "timestamp": utc_now().isoformat(),
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
  from src.core.runs import parse_raw_lines
  try:
    raw = path.read_bytes()
  except OSError:
    return []
  if not raw:
    return []
  return [event for event in parse_raw_lines(raw) if event.get("type") not in _TRANSCRIPT_SKIP_TYPES]


def _file_signature(path: Path) -> tuple[int, int] | None:
  try:
    st = os.stat(path)
  except OSError:
    return None
  return (st.st_mtime_ns, st.st_size)


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
      parts.append((entry.name, _file_signature(entry / "metadata.json"), _file_signature(entry / "events.jsonl")))
  parts.append(("chat", _file_signature(tree.sessions.get_chat_events_path(session_id))))
  return tuple(parts)


def build_worker_transcript_sync(tree, session_id: str) -> TranscriptEntry:
  """Build the worker transcript: one header per Run, its events, the close.

  The caller (API layer) has already established that *session_id* is a
  task-tree worker node; this builder reads only the run store and the node's
  chat facts.
  """
  runs = tree.runs.list_run_records_sync(session_id)
  facts_events = tree.runs.load_events_sync(session_id)
  from src.core.runs import read_host_boot_time
  host_boot = read_host_boot_time()
  states = {run.id: tree.runs.run_display_state(run, facts_events, host_boot) for run in runs}
  transcript: list[dict] = []
  for run in runs:
    state = states.get(run.id, "queued")
    run_events = _read_events(tree.runs.run_dir(session_id, run.id) / "events.jsonl")
    transcript.append(
        _run_header_event(run, state, _backend_label(tree.cfg, run.backend), _header_error(state, run_events)))
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


def drop_worker_transcript(session_id: str) -> None:
  """Forget a node's memoized transcript (its node is gone or archived)."""
  _worker_memo.drop(session_id)


# ---------------------------------------------------------------------------
# Legacy worker thread: threads/<thread_id>/data/events.jsonl
# ---------------------------------------------------------------------------


def _thread_dir(session_dir: Path, thread_id: str) -> Path:
  return session_dir / "threads" / thread_id


def thread_signature_sync(session_dir: Path, thread_id: str) -> tuple:
  """Stat-only identity of one legacy thread's metadata and events files."""
  thread_dir = _thread_dir(session_dir, thread_id)
  return (_file_signature(thread_dir / "metadata.json"), _file_signature(thread_dir / "data" / "events.jsonl"))


def _thread_state(meta: ThreadMetadata) -> str:
  """The thread's display state: its recorded status, liveness-checked while running."""
  if meta.status != "running":
    return str(meta.status)
  from src.core.runs import is_run_alive, read_host_boot_time
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
  started = meta.started_at.isoformat() if meta.started_at is not None else None
  thread_events = _read_events(events_path)
  transcript: list[dict] = [
      {
          "type": ET.RUN_HEADER,
          "run_id": meta.id,
          "kind": "thread",
          "backend": meta.backend or "",
          "backend_label": label,
          "state": state,
          "error": _header_error(state, thread_events),
          "started_at": started,
          "timestamp": started or utc_now().isoformat(),
      }
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
