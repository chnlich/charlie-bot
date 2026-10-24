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
transcript costs one scandir and a few stats.

Event ordinals are positions in the synthesized list, so the bootstrap tail,
the ``/events`` pagination pages, and the transcript poll all speak one cursor
space per node.
"""

from __future__ import annotations

import dataclasses
import datetime
import hashlib
import json
import pathlib

from src.infra import event_types as ET
from src.infra import memo, models
from src.runtime import message_aggregator, message_projection, runs, task_completion, task_prompts

# One entry per open worker page, bounded like the other read-path memos.
_WORKER_PROJECTION_MEMO_LIMIT = 32

_worker_memo: memo.BoundedMemo[str, TranscriptEntry] = memo.BoundedMemo(_WORKER_PROJECTION_MEMO_LIMIT)

# Run events whose only role is process bookkeeping, never a chat row: the
# native-session adoption signal opens a run interval for the stable-prefix
# scanner, which a worker transcript never closes, so it is filtered before
# the fold.
_TRANSCRIPT_SKIP_TYPES = frozenset({ET.SESSION_ATTACHED})

# The states a stop control can act on: a live process, or a queued
# reservation whose durable stop request the dispatch stage honors.
_SIGNALLABLE_STATES = frozenset({"running", "queued"})

# The states the header reads as a failure: the header then carries the Run's
# own error text.
_FAILED_STATES = frozenset({"failed", "interrupted", "attention"})


@dataclasses.dataclass
class TranscriptEntry:
  """One memoized transcript projection plus the facts its readers need."""

  # The client-facing revision: moves exactly when a full re-render (an API
  # ``reset``) is due — the run collection, each run's metadata, the chat
  # facts file, each run's state and the task state, plus every run's events
  # stat except the newest (an append re-derives the tail, it never moves
  # history before ``after``).
  revision: str
  # The memo's rebuild key over the FULL stat signature, including the newest
  # run's events stat: the summary's live rows (counts, last text, last tool)
  # must follow the streaming log, so an append rebuilds even though it does
  # not move the client-facing revision.
  rebuild_key: str
  events: list[dict]
  projection: message_projection.MessageProjection
  summary: dict
  active_run_id: str | None = None
  task_state: str = "open"
  states: dict[str, str] = dataclasses.field(default_factory=dict)


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
    started_at: datetime.datetime | None,
    terminal_at: datetime.datetime | None,
    task_spec_ref: str,
    launch_prompt_ref: str,
    withheld: str | None = None,
    tool_calls: int | None = None,
) -> dict:
  """The one header line that opens a Run transcript segment.

  The header carries one real time, from the projected record's own facts and
  never a clock read: the start while it ran, the terminal fact's time when it never started, else no
  time. A page-load or projection-build time would show a dead record a
  future-lying bubble (and once fed a negative duration).

  ``state`` stays the record's display state for every existing reader (the
  sidebar, the stop control, the dot color); ``launch_failed`` only renames
  the content word (message_aggregator) for a Run that failed before its agent
  process started — no started_at, no launch prompt ever written. The two refs
  feed the header's link row (gray placeholders when empty). ``withheld`` is a withheld Run's reason (the durable
  run_launch_withheld fact's own words); the content word renders it as
  "withheld · <reason>".
  ``tool_calls`` rides only a terminated Run's header (the run_tool_calls
  count, carrying the rendered tool rows it names); a running Run carries
  None — its count changes as it streams, so it lives in the summary.
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
      "tool_calls": tool_calls,
      "withheld": withheld,
      "started_at": started,
      "timestamp": started or terminal,
      "launched": started_at is not None,
      "launch_failed": state in _FAILED_STATES and started_at is None,
      "task_spec_ref": task_spec_ref,
      "launch_prompt_ref": launch_prompt_ref,
  }


def _run_end_event(run: models.RunRecord) -> dict:
  """The separator that closes one terminated Run's segment.

  The Run's own duration rides ``duration_seconds`` and the terminal fact's
  time the timestamp, so the run ended line reads like the
  chat's response-complete one. A Run that never launched (a withheld
  reservation) has no duration and no end time; it still closes its segment,
  as an empty one.
  """
  duration: int | None = None
  if run.started_at is not None and run.ended_at is not None:
    duration = int((run.ended_at - run.started_at).total_seconds())
  return {
      "type": ET.RUN_END,
      "run_id": run.id,
      "timestamp": (run.ended_at.isoformat() if run.ended_at is not None else None),
      "duration_seconds": duration,
  }


def _current_close(facts_events: list[dict]) -> dict | None:
  """The task_closed fact the task stands closed by, or None while it is open.

  That is the last task_closed fact, unless a task_reopened fact follows it.
  The delivery card and the summary card both read the task's close from
  here.
  """
  close = None
  for event in facts_events:
    kind = event.get("type")
    if kind == ET.TASK_CLOSED:
      close = event
    elif kind == ET.TASK_REOPENED:
      close = None
  return close


def _delivery_event(
    task_state: str, close: dict | None, facts_events: list[dict], run_records: list[models.RunRecord]) -> dict | None:
  """The closing delivery summary while the task stands closed.

  The summary, result refs and timestamp ride the current close
  (:func:`_current_close`); the four evidence links ride the delivered Run's
  record (newest successful run, else the newest run when a resolved task
  never recorded a success).
  """
  if close is None or not run_records:
    return None
  outcomes = {
      str(event.get("run_id")): str(event.get("outcome") or "")
      for event in facts_events
      if event.get("type") == ET.RUN_FINISHED
  }
  delivered = next((run for run in reversed(run_records) if outcomes.get(run.id) == "success"), run_records[-1])
  return {
      "type": ET.RUN_DELIVERY,
      "task_state": task_state,
      "summary": str(close.get("summary") or ""),
      "result_refs": [str(ref) for ref in (close.get("result_refs") or [])],
      "raw_log_ref": delivered.raw_log_ref or "",
      "events_ref": delivered.events_ref or "",
      "result_ref": delivered.result_ref or "",
      "repo_path": delivered.repo_path or "",
      "base_branch": delivered.base_branch or "",
      "branch_name": delivered.branch_name or "",
      "run_id": delivered.id,
      "timestamp": str(close["timestamp"]),
  }


def _revision(signature: tuple, states: dict[str, str], task_state: str) -> str:
  """The memo rebuild key: the full stat signature with every Run state and the
  task state, so any file move or state change rebuilds the projection."""
  payload = json.dumps(
      [[*item] if isinstance(item, tuple) else item for item in signature] + [sorted(states.items()), task_state],
      sort_keys=True,
      default=str)
  return hashlib.sha256(payload.encode()).hexdigest()[:24]


def _read_events(path: pathlib.Path) -> list[dict]:
  """One events.jsonl parsed; a missing or unreadable log is an empty segment."""
  try:
    raw = path.read_bytes()
  except OSError:
    return []
  if not raw:
    return []
  return [event for event in runs.parse_raw_lines(raw) if event.get("type") not in _TRANSCRIPT_SKIP_TYPES]


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
      parts.append(
          (
              entry.name, memo.stat_signature(entry / runs.RUN_METADATA_NAME),
              memo.stat_signature(entry / runs.RUN_EVENTS_NAME)))
  parts.append(("chat", memo.stat_signature(tree.session_events.get_chat_events_path(session_id))))
  return tuple(parts)


def _landed(close: dict | None) -> str | None:
  """The value after the ``landed:`` result ref of the current close, or None."""
  if close is None:
    return None
  landed = None
  for ref in close.get("result_refs") or []:
    ref = str(ref)
    if ref.startswith(task_completion.LANDING_REF_PREFIX):
      landed = ref[len(task_completion.LANDING_REF_PREFIX):]
  return landed


# The run_delivery fields the summary's ``delivery`` carries: the delivered
# Run and the evidence the Diff link reads.
_SUMMARY_DELIVERY_FIELDS = (
    "run_id", "raw_log_ref", "events_ref", "result_ref", "repo_path", "base_branch", "branch_name")


def _summary_links(tree, session_id: str, run: models.RunRecord | None) -> dict:
  """The newest Run's evidence refs, the header link row's source."""
  launch_prompt_path = tree.runs.run_dir(session_id, run.id) / task_prompts.LAUNCH_TEXT_FILENAME
  return {
      "task_spec_ref": run.task_spec_ref or "",
      # The launch prompt's presence is judged by the file (Runs older than
      # launch_prompt.md started without one), never by started_at.
      "launch_prompt_ref": str(launch_prompt_path) if launch_prompt_path.is_file() else "",
      "raw_log_ref": run.raw_log_ref or "",
      "events_ref": run.events_ref or "",
  }


def _build_summary(
    *,
    tree,
    session_id: str,
    task_state: str,
    close: dict | None,
    delivery: dict | None,
    facts_events: list[dict],
    run_records: list[models.RunRecord],
    states: dict[str, str],
    run_events: dict[str, list[dict]],
    run_tools: dict[str, int],
    run_causes: dict[str, dict],
) -> dict:
  """The worker page's summary card, in the plan 4.2 field table.

  The newest run drives the live fields: ``active`` spans only its
  ``running`` state — ``last_text``/``last_tool`` carry their own
  timestamps, so the card never needs a clock read from this side. ``runs``
  is one row per Run in record order. ``closing`` holds each role's newest
  Run's own words (message_aggregator.closing_text, the rule the reviewer's
  worker summary also reads). ``cause``
  is the newest failed Run's first cause line; ``stop`` the newest stop
  request fact. ``closed_at``, ``landed`` and ``delivery`` read the current
  close and the delivery card built from it, so all three are None while the
  task is open. Everything reads what the transcript build already parsed.
  """
  runs_rows = [
      {
          "run_id": run.id,
          "kind": run.kind,
          "backend_label": _backend_label(tree.cfg, run.backend),
          "state": states.get(run.id, "queued"),
          "started_at": run.started_at.isoformat() if run.started_at else None,
          "ended_at": run.ended_at.isoformat() if run.ended_at else None,
          "tool_calls": run_tools.get(run.id, 0),
      } for run in run_records
  ]
  newest = run_records[-1] if run_records else None
  active = None
  if newest is not None and states.get(newest.id) == "running":
    events = run_events.get(newest.id, [])
    active = {
        "run_id": newest.id,
        "started_at": newest.started_at.isoformat() if newest.started_at else None,
        "tool_calls": run_tools.get(newest.id, 0),
        "last_text": message_aggregator.run_last_assistant_text(events),
        "last_tool": message_aggregator.run_last_tool(events),
    }
  closing: dict[str, dict | None] = {}
  for kind in ("work", "review"):
    target = next((run for run in reversed(run_records) if run.kind == kind), None)
    text = (message_aggregator.run_last_text(run_events.get(target.id, [])) if target is not None else None)
    closing[kind] = (
        {
            "run_id": target.id,
            "text": text["text"] if text is not None else None
        } if target is not None else None)
  cause = None
  failed_run = next((run for run in reversed(run_records) if states.get(run.id) in _FAILED_STATES), None)
  if failed_run is not None and failed_run.id in run_causes:
    first = run_causes[failed_run.id]
    cause = {"run_id": failed_run.id, "text": first["first_line"], "timestamp": first["timestamp"]}
  stop = None
  for event in reversed(facts_events):
    if event.get("type") == ET.RUN_STOP_REQUESTED:
      stop = {
          "run_id": str(event.get("run_id") or ""),
          "request_id": str(event.get("request_id") or ""),
          "timestamp": str(event.get("timestamp") or ""),
      }
      break
  summary_delivery = None
  if delivery is not None:
    summary_delivery = {field: delivery[field] for field in _SUMMARY_DELIVERY_FIELDS}
  return {
      "task_state": task_state,
      "closed_at": str(close["timestamp"]) if close is not None else None,
      "landed": _landed(close),
      "delivery": summary_delivery,
      "runs": runs_rows,
      "active": active,
      "closing": closing,
      "cause": cause,
      "stop": stop,
      "links": (_summary_links(tree, session_id, newest) if newest is not None else {}),
  }


def build_worker_transcript_sync(tree, session_id: str) -> TranscriptEntry:
  """Build the worker transcript: one header per Run, its events, the close,
  and one summary for the page's For-you card.

  The caller (API layer) has already established that *session_id* is a
  task-tree worker node; this builder reads only the run store and the node's
  chat facts.
  """
  run_records = tree.runs.list_run_records_sync(session_id)
  facts_events = tree.runs.load_events_sync(session_id)
  host_boot = runs.read_host_boot_time()
  states = {run.id: tree.runs.run_display_state(run, facts_events, host_boot) for run in run_records}
  transcript: list[dict] = []
  run_events_map: dict[str, list[dict]] = {}
  run_tools: dict[str, int] = {}
  run_causes: dict[str, dict] = {}
  for run in run_records:
    state = states.get(run.id, "queued")
    run_dir = tree.runs.run_dir(session_id, run.id)
    run_events = _read_events(run_dir / runs.RUN_EVENTS_NAME)
    # A failed Run's header carries its cause text (the one cause rule,
    # message_aggregator.run_cause) in full — the first event that spoke is
    # dropped from this projection only, events.jsonl keeps every line.
    error = ""
    if state in _FAILED_STATES:
      cause = message_aggregator.run_cause(run_events)
      if cause is not None:
        text, first_line, error_index, cause_at = cause
        error = text
        run_causes[run.id] = {"first_line": first_line, "timestamp": cause_at}
        run_events = run_events[:error_index] + run_events[error_index + 1:]
    # The summary counts and rows read the events before type rewriting;
    # the projection's error rows read them after.
    run_tools[run.id] = message_aggregator.run_tool_call_count(run_events)
    run_events_map[run.id] = list(run_events)
    projected_events = [
        {
            **event, "type": ET.RUN_ERROR
        } if event.get("type") == ET.ERROR else event for event in run_events
    ]
    launch_prompt_path = run_dir / task_prompts.LAUNCH_TEXT_FILENAME
    tool_calls = run_tools[run.id]
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
            launch_prompt_ref=str(launch_prompt_path) if launch_prompt_path.is_file() else "",
            withheld=(tree.runs.withheld_reason(facts_events, run.id) if state == "withheld" else None),
            tool_calls=(tool_calls if state not in _SIGNALLABLE_STATES else None)))
    transcript.extend(projected_events)
    if state not in _SIGNALLABLE_STATES:
      transcript.append(_run_end_event(run))
  task_state = tree.task_state(session_id)
  close = _current_close(facts_events)
  delivery = _delivery_event(task_state, close, facts_events, run_records)
  if delivery is not None:
    transcript.append(delivery)
  active_run_id = next((run.id for run in reversed(run_records) if states.get(run.id) in _SIGNALLABLE_STATES), None)
  signature = worker_signature_sync(tree, session_id)
  summary = _build_summary(
      tree=tree,
      session_id=session_id,
      task_state=task_state,
      close=close,
      delivery=delivery,
      facts_events=facts_events,
      run_records=run_records,
      states=states,
      run_events=run_events_map,
      run_tools=run_tools,
      run_causes=run_causes)
  return TranscriptEntry(
      revision=_client_revision(signature, run_records, states, task_state),
      rebuild_key=_revision(signature, states, task_state),
      events=transcript,
      projection=message_projection.MessageProjection(list(transcript)),
      summary=summary,
      active_run_id=active_run_id,
      task_state=task_state,
      states=states)


def _client_revision_input(
    signature: tuple,
    run_records: list[models.RunRecord],
    states: dict[str, str],
    task_state: str,
) -> list:
  """The revision's inputs, carried from the stat scan in record order.

  One entry per re-render trigger: the run collection, each run's metadata
  stat, the chat facts stat, each run's state, the task state, and every
  run's events stat except the newest Run's events stat while that Run is
  running. A live Run's history before ``after`` is immutable, so its appends
  ride the committed list, never a reset.
  """
  stats = {part[0]: (part[1], part[2]) for part in signature[:-1]}
  meta_stats: dict[str, object] = {}
  event_stats: dict[str, object] = {}
  for run in run_records:
    meta_sig, events_sig = stats[run.id]
    meta_stats[run.id] = meta_sig
    event_stats[run.id] = events_sig
  event_stats_for_revision = [
      event_stats[run.id]
      for index, run in enumerate(run_records)
      if index != len(run_records) - 1 or states.get(run.id) != "running"
  ]
  return [
      [run.id for run in run_records],
      [meta_stats[run.id] for run in run_records],
      signature[-1],
      [states.get(run.id) for run in run_records],
      task_state,
      event_stats_for_revision,
  ]


def _client_revision(
    signature: tuple, run_records: list[models.RunRecord], states: dict[str, str], task_state: str) -> str:
  """The client revision: moves exactly when a re-render (an API ``reset``) is
  due, over the inputs :func:`_client_revision_input` names."""
  payload = json.dumps(_client_revision_input(signature, run_records, states, task_state), sort_keys=True, default=str)
  return hashlib.sha256(payload.encode()).hexdigest()[:24]


def load_worker_transcript(tree, session_id: str) -> TranscriptEntry:
  """The memoized worker transcript; a moved signature or state rebuilds.

  The rebuild check folds the CURRENT task state, not the cached one — a task
  close changes only that input, and a memo check over the cached value would
  never observe it (the delivered close would wait for an unrelated run-file
  move to surface). The facts fold is memoized, so the read is in-memory. The
  check runs over ``rebuild_key`` (the full stat signature); the returned
  entry's ``revision`` is the narrow, append-stable one, so the poll that
  follows a streaming append serves its new messages incrementally and a
  fresh summary beside them.
  """
  signature = worker_signature_sync(tree, session_id)
  task_state = tree.task_state(session_id)
  cached = _worker_memo.get(session_id)
  if cached is not None and cached.rebuild_key == _revision(signature, cached.states, task_state):
    return cached
  entry = build_worker_transcript_sync(tree, session_id)
  _worker_memo.store(session_id, entry)
  return entry
