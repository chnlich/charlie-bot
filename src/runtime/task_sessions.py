"""Task-tree owner: task metadata mutations, tree validation, and derived queries.

This module is the single owner of task metadata, of the task-tree
relation (``task_parent_id`` edges over flat ``sessions/<id>`` directories),
and of the derived task projection (``task_state`` / ``work_state`` / archive
visibility / subtree counts). The session metadata, chat events, listings and
lifecycle writes live in the ``src.runtime.session_*`` blocks.

Guarantees the delivery stage pins down:

- Every tree mutation, input/event seam write, Run registration and (future)
  completion check holds the one short control write lock
  (:attr:`TaskTreeManager.control_lock`); subprocess/network/token streaming
  stays outside it.
- A task create binds (parent, request_id) to a stable UUID, builds metadata
  plus its creation fact in a temp directory, and publishes the whole node
  atomically; a replayed request returns the original product, including after
  a fresh manager is constructed from disk.
- Task state and work state are rebuilt from durable facts (task_closed /
  task_reopened / run_finished / run records) — no persisted lifecycle state
  machine and no durable aggregate tree status exists.
- Input delivery (``src.runtime.session_dispatch``) and completion guards
  (``src.runtime.task_completion``) own their policies and join the same control
  lock; this module hosts their wiring, the fact-history reader they share,
  and the pending-input blocker hook they answer.
"""

from __future__ import annotations

import asyncio
import dataclasses
import datetime
import os
import pathlib
import re
import shutil
import time
import uuid
from collections.abc import Awaitable, Callable
from typing import Any

import orjson

from src.infra import config, json_utils, metadata_slots, models, ndjson, tasks
from src.infra import event_types as ET
from src.runtime import (
    chat_events,
    control_events,
    control_sink,
    run_token,
    runs,
    session_anchors,
    session_dispatch,
    session_events,
    session_fork,
    session_lifecycle,
    session_listing,
    session_sidebar,
    session_store,
    session_successor,
    takeoff_gate,
    task_completion,
    task_errors,
    thinking_state,
)
from src.runtime.hooks import sequence_controllers

PROMPT_BODIES_DIR_NAME = "prompt_bodies"

# The rebuildable index self-heals on this cadence even when nothing the owner
# wrote moved the sessions root: one metadata walk per TTL per process is the
# bound an out-of-band edit (none exists in the single-service model) could
# stay invisible for.
_TREE_INDEX_TTL_SECONDS = 2.0

# Refusal wording shared by the raises that own the contracts and their routes in
# src/runtime/api/sessions.py, which carry it as the client-visible detail: the create route's v2
# pre-check and legacy-shape guard read the first two, the archive route's operator guard the third.
TASK_CREATE_REQUEST_ID_REQUIRED = "request_id is required for task creation"
AGENT_CREATE_SCOPE_REFUSAL = "an agent may only create a task directly under its own open manager task"


def closed_ancestors_blocker(closed_ids: list[str]) -> str:
  """The 409 blocker sentence naming a node's closed ancestor tasks."""
  return f"closed ancestor task(s): {', '.join(closed_ids)}"


@dataclasses.dataclass
class _TaskFacts:
  """The derived facts one session's full event history folds to.

  The fold is the single derived-fact owner for the tree projection, the
  input dispatcher, and the completion owner: task lifecycle, run outcomes,
  input candidacy with its boundary, close and close-request facts, and
  delivered child reports all come from this one pass over the durable
  events (archived segments included).
  """
  task_state: str = "open"
  run_outcomes: dict[str, str] = dataclasses.field(default_factory=dict)
  # Each run's first run_finished fact's absolute log position (the durable
  # order; models.RunRecord carries no creation timestamp).
  run_finish_positions: dict[str, int] = dataclasses.field(default_factory=dict)
  # Input ids a successful run_finished acknowledged.
  confirmed_input_ids: set[str] = dataclasses.field(default_factory=set)
  # Input events inside the valid boundary (pre confirmation/claim filtering).
  input_candidates: list[dict] = dataclasses.field(default_factory=list)
  # Old pending input ids the task_imported boundary explicitly admits.
  imported_pending_ids: frozenset[str] = frozenset()
  # Absolute history position of the latest input boundary fact (creation,
  # import, close, or reopen); None before either fact exists.
  boundary_index: int | None = None
  # Whether inputs are admitted after the current boundary: True from
  # creation/import/reopen, False from a close (a closed node admits nothing;
  # arrivals during closure are history only).
  boundary_open: bool = True
  close_events: list[dict] = dataclasses.field(default_factory=list)
  close_requests: list[dict] = dataclasses.field(default_factory=list)
  # (child_session_id, child_event_id) pairs this session's log has received.
  delivered_reports: set[tuple[str, str]] = dataclasses.field(default_factory=set)
  # Every event id in the folded history (identity dedup for reports/inputs).
  event_ids: set[str] = dataclasses.field(default_factory=set)
  events_by_id: dict[str, dict] = dataclasses.field(default_factory=dict)
  covered: int = 0


# The history facts whose newest one fixes a node's task state.
_LIFECYCLE_STATE_TYPES = frozenset({ET.CLONE_START, ET.TASK_CLOSED, ET.TASK_REOPENED})
# The history facts a node's Run verdicts read: terminal outcomes and stop requests.
_RUN_FACT_TYPES = frozenset({ET.RUN_FINISHED, ET.RUN_STOP_REQUESTED})
# The tree's history walks release the GIL every this many lines. A walk holds the GIL in pure Python between
# file reads, so the event loop's thread would otherwise wait out the interpreter's forced switch on each request.
_TREE_WALK_GIL_YIELD_LINES = 256


def _task_state_of_lifecycle(event: dict) -> str:
  """The task state one lifecycle fact sets (the shared half of _fold_task_events)."""
  if event["type"] == ET.TASK_CLOSED:
    return str(event.get("outcome") or "completed")
  return "open"


def _fold_task_events(facts: _TaskFacts, events: list[dict], index_offset: int) -> _TaskFacts:
  """Fold *events* (absolute history positions from *index_offset*) into *facts*.

  Duplicate event ids are folded once, so an overlapping archived/live read
  can double-present a segment without double-counting its facts.
  """
  for position, event in enumerate(events):
    event_id = event.get("id")
    if event_id is not None:
      if event_id in facts.event_ids:
        continue
      facts.event_ids.add(event_id)
      facts.events_by_id[event_id] = event
    etype = event.get("type")
    absolute = index_offset + position
    if etype == ET.CLONE_START:
      # A fork/elone child's log opens with the parent's copied lines. Whatever
      # lifecycle, boundary, close or pending-input fact they carry belongs to
      # the parent; the child's own task_created after this marker starts its
      # lifecycle.
      facts.task_state = _task_state_of_lifecycle(event)
      facts.boundary_index = None
      facts.boundary_open = True
      facts.input_candidates = []
      facts.imported_pending_ids = frozenset()
      facts.close_events = []
      facts.close_requests = []
      facts.delivered_reports = set()
    elif etype == ET.TASK_CREATED:
      if facts.boundary_index is None:
        facts.boundary_index = absolute
        # Fresh tasks admit post-creation inputs only: anything folded before
        # the boundary existed is history, never a candidate.
        facts.input_candidates = []
    elif etype == ET.TASK_IMPORTED:
      listed: list[str] = []
      for entry in event.get("pending_inputs") or []:
        input_id = entry.get("input_id") if isinstance(entry, dict) else None
        if not isinstance(input_id, str) or not input_id:
          raise ValueError(f"task_imported event {event_id!r} lists a pending input without a stable input_id")
        listed.append(input_id)
      facts.boundary_index = absolute
      facts.imported_pending_ids = frozenset(listed)
      # The boundary moved: imported tasks admit post-import input plus ONLY
      # the old pending inputs the boundary explicitly lists — pre-boundary
      # candidacy narrows to that declared set, never the whole history.
      facts.input_candidates = [
          e for e in facts.events_by_id.values() if _admits_input_type(e) and e.get("id") in facts.imported_pending_ids
      ]
    elif etype == ET.TASK_CLOSED:
      facts.task_state = _task_state_of_lifecycle(event)
      facts.close_events.append(event)
      # The close is an input boundary that closes the boundary: every
      # unhandled candidate and every arrival while the node stays closed is
      # history only. A reopen below opens a fresh boundary, so nothing before
      # it returns to candidacy.
      facts.input_candidates = []
      facts.imported_pending_ids = frozenset()
      facts.boundary_index = absolute
      facts.boundary_open = False
    elif etype == ET.TASK_REOPENED:
      facts.task_state = _task_state_of_lifecycle(event)
      # The reopen opens a fresh input boundary: post-reopen inputs are
      # candidates; close-time candidates and closed-period arrivals stay
      # history.
      facts.boundary_index = absolute
      facts.boundary_open = True
    elif etype == ET.TASK_CLOSE_REQUESTED:
      facts.close_requests.append(event)
    elif etype == ET.RUN_FINISHED:
      run_id = event.get("run_id")
      if isinstance(run_id, str):
        facts.run_outcomes[run_id] = str(event.get("outcome"))
        if run_id not in facts.run_finish_positions:
          facts.run_finish_positions[run_id] = absolute
        if event.get("outcome") == "success":
          ids = event.get("input_event_ids")
          if isinstance(ids, list):
            facts.confirmed_input_ids.update(str(i) for i in ids)
    elif etype == ET.TASK_INPUT_ACKNOWLEDGED:
      ids = event.get("input_ids")
      if isinstance(ids, list):
        facts.confirmed_input_ids.update(str(i) for i in ids)
    elif etype == ET.CHILD_REPORT:
      child_session_id = event.get("child_session_id")
      child_event_id = event.get("child_event_id")
      if isinstance(child_session_id, str) and isinstance(child_event_id, str):
        facts.delivered_reports.add((child_session_id, child_event_id))
    if _admits_input_type(event) and facts.boundary_open and (
        facts.boundary_index is None or absolute > facts.boundary_index or
        (event_id is not None and event_id in facts.imported_pending_ids)):
      facts.input_candidates.append(event)
  return facts


def _admits_input_type(event: dict) -> bool:
  """The fold's input-type admission.

  The admitted types are the one INPUT_EVENT_TYPES definition
  (src/runtime/session_dispatch.py) — the same set ``admit_input`` accepts.
  Agent messages, scheduled triggers, and child reports are input by type; a
  USER event is input only when it is a real user message — the Claude CLI
  persists each tool result as a user-type event with list content, and that
  echo is tool output no round can ever confirm, not a message.
  """
  etype = event.get("type")
  if etype not in session_dispatch.INPUT_EVENT_TYPES:
    return False
  if etype == ET.USER:
    return ET.is_real_user_message(event)
  return True


@dataclasses.dataclass(frozen=True)
class TaskTreeActivity:
  """One task-tree node's derived activity: the sidebar's truth for the node.

  ``has_running_tasks`` is true exactly while one of the node's Runs is live
  (recorded pid alive, no terminal fact); ``work_state`` is the node's
  fact-derived work verdict (idle | running | waiting) — waiting means a
  queued Run on an open task. Both come from one derivation —
  :func:`derive_task_tree_activity` — that ``TaskTreeManager.activity_of``
  and the sidebar's task-tree probe share.
  """
  has_running_tasks: bool
  work_state: models.WorkState


def derive_task_tree_activity(
    run_records: list[models.RunRecord],
    facts: runs.RunFacts,
    host_boot_time: Callable[[], datetime.datetime],
    task_open: Callable[[], bool],
) -> TaskTreeActivity:
  """The single owner of a task-tree node's activity rules.

  Reads only durable facts (the run records, and the node's folded Run facts:
  terminal outcomes and stop requests) plus process liveness for Runs that
  still lack a terminal fact — /proc is never consulted for a terminal or
  queued Run. A run record with ``ended_at`` is settled without its fact: the
  finish path writes ``ended_at`` only after the terminal fact lands. The host
  boot time and the task's openness are read lazily, only when a Run needs
  them. Running work wins over waiting work; every Run with a terminal fact
  contributes nothing, and so does a launched Run whose process is dead (its
  recovery is the completion/cancellation blockers' job, not a sidebar
  verdict).

  Waiting means a queued Run on an open task: ``task_open`` gates the queued
  verdict, because ``execute_run`` withholds every launch on a non-open task,
  so a closed task's queued Run never starts and must not hold the sidebar's
  clock. The running verdict is not gated: closure is refused while a Run is
  active, so a closed task cannot hold a live Run.
  """
  if not run_records:
    return TaskTreeActivity(has_running_tasks=False, work_state="idle")
  verdicts: list[str] = []
  has_running = False
  boot: datetime.datetime | None = None
  for run in run_records:
    if run.ended_at is not None or facts.outcomes.get(run.id) is not None:
      continue  # a terminal fact of any outcome is a settled Run, not activity
    if run.pid is None:
      if facts.stop_requests.get(run.id):
        continue  # a stopped queued run is resolved-by-request, not waiting work
      if not task_open():
        continue  # a closed task's queued run never launches, so it is not waiting work
      verdicts.append("waiting")  # queued on an open task: retains its inputs for later dispatch
      continue
    # Liveness is judged only here, only for a launched Run without a terminal
    # fact — the sole case where a /proc read can change the verdict.
    if boot is None:
      boot = host_boot_time()
    if runs.is_run_alive(run.pid, run.pid_start, run.started_at, boot):
      has_running = True
      verdicts.append("running")
    # A launched Run nobody observed exiting paints nothing: subagent failures
    # are routine and the manager resolves them by re-delegating, so a dead
    # process is not work the sidebar flags.
  work_state: models.WorkState = "idle"
  for state in ("running", "waiting"):
    if state in verdicts:
      work_state = state  # type: ignore[assignment]
      break
  return TaskTreeActivity(has_running_tasks=has_running, work_state=work_state)


def _live_end(identity: tuple[int, int, int] | None) -> int:
  """The live-file byte offset a history identity covers (0 while the file does not exist)."""
  return 0 if identity is None else identity[1]


class _ColdReadError(Exception):
  """A warm-only read pass met a node whose verdict is not memoized, so the pass must run off the loop."""


class _NodeReads:
  """One read pass over task-tree nodes: each node's lifecycle state and activity, read once.

  A row reads its node from several places: its own state and activity, its
  parent's open and running counts, and the archive inheritance of its subtree.
  Every one of those reads takes the verdict this pass holds for the node, so a
  concurrent append lands wholly before or wholly after the pass. The node's
  history identity is taken before its first read, and the live-file reads stop
  at that identity's size, so a verdict is exactly the verdict of the content its
  identity names.

  A warm-only pass answers from the memos alone: a node whose verdict is not
  memoized at its current identity raises _ColdReadError, and the caller reruns the
  pass on a worker thread. The loop serves a warm read with a stat and a dict
  lookup, and never reads a history file.
  """

  def __init__(self, tree: TaskTreeManager, *, warm_only: bool = False) -> None:
    self._tree = tree
    self._warm_only = warm_only
    # Read before any run record: the generation keys every activity verdict this pass stores.
    self._generation = tree.runs.records_generation
    self._identities: dict[str, tuple[int, int, int] | None] = {}
    self._states: dict[str, str] = {}
    self._activity: dict[str, TaskTreeActivity] = {}

  def _identity(self, session_id: str) -> tuple[int, int, int] | None:
    if session_id not in self._identities:
      self._identities[session_id] = self._tree._history_identity(session_id)
    return self._identities[session_id]

  def task_state(self, session_id: str) -> str:
    """The node's lifecycle state from its history tail (see TaskTreeManager._task_state_fast)."""
    if session_id not in self._states:
      identity = self._identity(session_id)
      if self._warm_only:
        state = self._tree._memoized_task_state(session_id, identity)
        if state is None:
          raise _ColdReadError(session_id)
      else:
        state = self._tree._task_state_at(session_id, identity)
      self._states[session_id] = state
    return self._states[session_id]

  def activity(self, session_id: str) -> TaskTreeActivity:
    """The node's activity; a queued Run reads this pass's task state for its gate."""
    if session_id not in self._activity:
      identity = self._identity(session_id)
      if self._warm_only:
        activity = self._tree._memoized_activity(session_id, identity, self._generation)
        if activity is None:
          raise _ColdReadError(session_id)
      else:
        activity = self._tree._activity_at(
            session_id,
            identity,
            self._generation,
            task_open=lambda: self.task_state(session_id) == "open",
        )
      self._activity[session_id] = activity
    return self._activity[session_id]


class TaskTreeManager:
  """The task-tree record owner wired over the session blocks it reads and installs hooks on."""

  def __init__(
      self,
      cfg: config.CharlieBotConfig,
      store: session_store.SessionStore,
      events: session_events.SessionEvents,
      sidebar: session_sidebar.SessionSidebar,
      listing: session_listing.SessionListing,
      lifecycle: session_lifecycle.SessionLifecycle,
      fork: session_fork.SessionFork,
      anchors: session_anchors.SessionAnchors,
      successor: session_successor.SessionSuccessor,
  ) -> None:
    self._cfg = cfg
    self.session_store = store
    self._lifecycle = lifecycle
    self.session_anchors = anchors
    self.session_events = events
    self.session_successor = successor
    self.control_lock = asyncio.Lock()
    # One tree-row computation at a time: a burst of pages would otherwise run the same
    # history reads on parallel threads and starve the event loop of the GIL. A page
    # behind the first finds the memos the first left, so it costs a few stats.
    self._tree_rows_lock = asyncio.Lock()
    self.events = control_sink.ControlEventSink(self.session_events)
    self.runs = runs.RunStore(cfg.sessions_dir, self.control_lock, self.events)
    # The run owner's terminal/stop/identity reads see the full fact history
    # (archived segments included), so a rotated acknowledgement never un-dones
    # itself and a repeat finish stays idempotent across rotation.
    self.runs.set_fact_history_loader(self.fact_history)
    # The run owner's launch/finish paths push a worker Run's busy interval
    # (the node's thinking_since) and its display backend into thinking_state.
    # The notification is the tree owner's because only the tree index knows
    # which nodes are workers.
    self.runs.set_liveness_notifier(self._note_run_liveness)
    self.dispatch = session_dispatch.TaskInputDispatcher(self)
    self.completion = task_completion.TaskCompletionManager(self)
    # The pending-input blockers of one session ([] when none): the structural
    # guard seam the input dispatcher answers.
    self.pending_input_blockers: Callable[[str], list[str]] | None = self.dispatch.pending_input_blockers
    # Block-level writes that move a tree-projection input (the unread
    # flag in _set_unread_flag, a forked session's creation) drop this tree's
    # rebuildable index through the hook registered here — the same policy
    # _save_meta applies to its own metadata writes — so a tree page read after
    # the write never serves the stale state a missed broadcast would have left
    # standing.
    self._lifecycle.tree_index_invalidator = self.invalidate_tree_index
    fork.tree_index_invalidator = self.invalidate_tree_index
    # The fork consumes this tree through its TaskTreeOwner protocol (clone
    # validation, the elone replacement); the last-built tree over one fork
    # owns it, the same rule the invalidator rides.
    fork.tree = self
    # The session lists read stored status; the archive of a task node is a
    # derived fact (archived_of, subtree inheritance included). The overlay
    # lets the sidebar's active list drop a delivered worker — and, with it,
    # every descendant of an archived ancestor — while its Archived list shows
    # them, with no status write.
    listing.archive_overlay = self.derived_archived_ids
    # The sidebar's probe derives a task-tree node's activity through this
    # hook — the tree's own derivation, never a second copy of the rules.
    sidebar.task_tree_activity = self.activity_pair_of
    self._index: tuple[_TreeIndex, float] | None = None
    self._index_generation = 0
    self._index_build_task: asyncio.Task[_TreeIndex] | None = None
    self._index_build_generation = -1
    # The full fold's memo (task_state, facts_of): lifecycle decisions and
    # the input/completion owners read it, so it stays the list-identity fold.
    self._facts_memo: dict[str, tuple[list[dict], int, _TaskFacts]] = {}
    # Every memo below the full fold is keyed by the node's history identity
    # (inode, size, mtime) and holds small verdicts only, never parsed events.
    # The tree path writes these from its worker thread: a key taken before the
    # read can only trail the content it stores, so a racing append re-derives.
    self._state_tail_memo: dict[str, tuple[tuple[int, int, int] | None, str]] = {}
    # Run facts (terminal outcomes and stop requests) folded from the node's
    # history by identity; a generation bump re-derives the verdict from these
    # without another history scan.
    self._outcomes_memo: dict[str, tuple[tuple[int, int, int] | None, runs.RunFacts]] = {}
    # Activity cells: (records_generation, history identity, verdict). A verdict
    # that consulted /proc is never stored (see _activity_at).
    self._activity_memo: dict[str, tuple[int, tuple[int, int, int] | None, TaskTreeActivity]] = {}
    self._prompt_bodies_dir = cfg.charliebot_home / PROMPT_BODIES_DIR_NAME

  @property
  def prompt_bodies_dir(self) -> pathlib.Path:
    """The immutable local-rule body store (prompt_bodies/<sha256>.md) in the selected home."""
    return self._prompt_bodies_dir

  @property
  def cfg(self) -> config.CharlieBotConfig:
    """The config this tree was built over (display-label resolution reads it)."""
    return self._cfg

  # ------------------------------------------------------------------
  # Metadata reads/writes (single owner of the v2 fields)
  # ------------------------------------------------------------------

  async def load_meta(self, session_id: str) -> models.SessionMetadata | None:
    return await self.session_store.get_session(session_id)

  def _require_task(self, meta: models.SessionMetadata | None, session_id: str) -> models.SessionMetadata:
    if meta is None:
      raise task_errors.TaskNotFoundError(f"task {session_id} not found")
    return meta

  async def load_task_meta(self, session_id: str) -> models.SessionMetadata:
    """The session's metadata, required to be a task-tree node.

    Raises TaskNotFoundError for an absent session.
    """
    return self._require_task(await self.load_meta(session_id), session_id)

  async def _save_meta(self, meta: models.SessionMetadata) -> None:
    meta.updated_at = models.utc_now()
    await self.session_store.save_metadata(meta)
    self._invalidate_index()  # any metadata write may move the projection inputs

  # ------------------------------------------------------------------
  # Tree index (rebuildable)
  # ------------------------------------------------------------------

  async def _get_index(self, *, force: bool = False) -> _TreeIndex:
    now = time.monotonic()
    if not force and self._index is not None and now - self._index[1] < _TREE_INDEX_TTL_SECONDS:
      return self._index[0]
    generation = self._index_generation
    task = self._index_build_task
    if task is not None and self._index_build_generation == generation:
      # Every reader invalidated by the same write shares one build: a burst of
      # concurrent readers each paying its own full build multiplies the one
      # rebuild's wall across every request the write touches.
      if task.get_loop() is asyncio.get_running_loop():
        return await task
      # The marker can hold a build still pending on a loop that has closed
      # (TestClient's portal loop, in the mixed-loop tests); awaiting a
      # foreign-loop task raises, so this reader rebuilds on its own loop.
      self._index_build_task = None
    # The metadata snapshot resolves on the loop through the shared per-entry
    # check (_fresh_cached_meta), so the thread build reads a file only for a
    # session no authoritative entry covers (cold cache, out-of-band create).
    cached_metas = self.session_store.fresh_cached_metas()
    task = tasks.create_logged_task(
        asyncio.to_thread(self._build_index_sync, cached_metas), name="task-tree-index-build")
    self._index_build_task = task
    self._index_build_generation = generation
    try:
      index = await task
    finally:
      # The task removes itself from the marker on completion, success or
      # failure, so a later read retries fresh instead of inheriting a stale
      # failure; a newer build owning the marker must not be clobbered.
      if self._index_build_task is task:
        self._index_build_task = None
    if self._index_generation == generation:
      self._index = (index, now)
    # A structural write landing mid-build bumped the generation: the build's
    # snapshot is still a valid pre- (or post-) write read for THIS caller,
    # but it must never be installed over the newer invalidation.
    return index

  def _invalidate_index(self) -> None:
    """Drop the cached index and bump the generation, so an asynchronous
    build in flight cannot install a stale result over this write."""
    self._index = None
    self._index_generation += 1

  def invalidate_tree_index(self) -> None:
    """A session block's write (legacy rename) changed tree inputs.

    Tree-node facts normally mutate through this owner (which invalidates in
    _save_meta); the legacy rename path writes through the lifecycle block and
    must drop the projection cache here or tree_page serves a stale name."""
    self._invalidate_index()

  def _build_index_sync(self, cached_metas: dict[str, models.SessionMetadata]) -> _TreeIndex:
    sessions_dir = self._cfg.sessions_dir
    root_sig = (0, 0)
    try:
      st = os.stat(sessions_dir)
      root_sig = (st.st_mtime_ns, st.st_size)
    except OSError as e:
      raise RuntimeError(f"sessions dir unreadable at {sessions_dir}: {e}") from e
    metas: dict[str, models.SessionMetadata] = {}
    try:
      entries = sorted(e.name for e in os.scandir(sessions_dir) if e.is_dir())
    except OSError as e:
      raise RuntimeError(f"sessions dir unscannable at {sessions_dir}: {e}") from e
    for name in entries:
      if name.startswith(".task-") and name.endswith(".tmp"):
        continue  # an unpublished create's staging directory is never a node
      meta = cached_metas.get(name)
      if meta is not None:
        metas[name] = meta
        continue
      # No authoritative entry covers this name (cold cache, out-of-band
      # create): the strict read is the tree's own contract — an unparseable
      # file fails the build loud, where the listings readers drop and log.
      path = sessions_dir / name / runs.METADATA_NAME
      try:
        raw = path.read_text(encoding="utf-8")
      except OSError:
        continue  # session dir without (yet readable) metadata: not a tree node
      try:
        metas[name] = models.validate_session_metadata(raw, str(path))
      except ValueError as e:
        raise RuntimeError(f"session metadata unparseable at {path}: {e}") from e
    children: dict[str | None, list[str]] = {}
    task_nodes: list[tuple[str, models.SessionMetadata]] = []
    for sid, meta in metas.items():
      task_nodes.append((sid, meta))
      children.setdefault(meta.task_parent_id, []).append(sid)
    for kids in children.values():
      kids.sort(key=lambda sid: (metas[sid].created_at, sid))
    # The revision covers every input of archive membership (the tree page's
    # row filter), so a facts-driven membership change during pagination is
    # a visible 409 instead of a silently omitted or repeated row. Task nodes
    # record their EFFECTIVE archive value (subtree inheritance through
    # archived_of), so an ancestor's flip still moves the revision — a legacy
    # parent's archived status among them, which this loop skips entirely.
    index = _TreeIndex(metas=metas, children=children, revision="", root_sig=root_sig)
    memo: dict[str, bool] = {}
    reads = _NodeReads(self)
    structural: list[str] = []
    for sid, meta in task_nodes:
      state = reads.task_state(sid)
      structural.append(
          f"{sid}|{meta.task_parent_id or ''}|{meta.profile}|{meta.status.value}"
          f"|{state}|{self._archived_of_pass(index, meta, memo, reads)}")
    # The placeholder revision above is an input only to the shared-memo pass,
    # which never reads it; the real one lands before the index is published.
    index.revision = control_events.sha256_hex("\n".join(sorted(structural)))
    return index

  def _index_meta(self, index: _TreeIndex, session_id: str) -> models.SessionMetadata:
    meta = index.metas.get(session_id)
    if meta is None:
      raise task_errors.TaskNotFoundError(f"task {session_id} not found")
    return meta

  def _children_of(self, index: _TreeIndex, session_id: str | None) -> list[str]:
    return list(index.children.get(session_id, []))

  def _descendants(self, index: _TreeIndex, session_id: str, reads: _NodeReads | None = None) -> list[str]:
    """All transitive descendants, archived ones included, cycle-guarded (a cycle here is corrupted data, not a tree).

    The size cap counts only non-archived descendants: an archived node is one the
    stored status or the derived archive (archived_of) marks, and a closed subtree
    of any size is still walkable. One archive memo spans the walk, keeping it linear.
    """
    out: list[str] = []
    open_count = 0
    stack = list(self._children_of(index, session_id))
    seen = {session_id}
    memo: dict[str, bool] = {}
    reads = _NodeReads(self) if reads is None else reads
    while stack:
      sid = stack.pop()
      if sid in seen:
        raise task_errors.TaskConflictError([f"task relation cycle through {sid}"])
      seen.add(sid)
      out.append(sid)
      stack.extend(self._children_of(index, sid))
      meta = self._index_meta(index, sid)
      if meta.status != models.SessionStatus.ARCHIVED and not self._archived_of_pass(index, meta, memo, reads):
        open_count += 1
        if open_count > task_errors.DESCENDANT_NODE_LIMIT:
          raise task_errors.TaskConflictError(
              [f"subtree of {session_id} exceeds {task_errors.DESCENDANT_NODE_LIMIT} non-archived nodes"])
    return out

  def _ancestors(self, index: _TreeIndex, session_id: str) -> list[models.SessionMetadata]:
    """The task-parent chain above *session_id*, nearest first (cycle-guarded)."""
    chain: list[models.SessionMetadata] = []
    seen = {session_id}
    current = self._index_meta(index, session_id)
    while current.task_parent_id is not None:
      if current.task_parent_id in seen:
        raise task_errors.TaskConflictError([f"task relation cycle through {current.task_parent_id}"])
      seen.add(current.task_parent_id)
      current = self._index_meta(index, current.task_parent_id)
      chain.append(current)
      if len(chain) > task_errors.ANCESTOR_HOP_LIMIT:
        raise task_errors.TaskConflictError(
            [f"ancestor chain of {session_id} exceeds {task_errors.ANCESTOR_HOP_LIMIT} hops"])
    return chain

  async def ancestor_ids(self, session_id: str) -> list[str]:
    """The ids of the task-parent chain above *session_id*, nearest first."""
    index = await self._get_index()
    return [meta.id for meta in self._ancestors(index, session_id)]

  async def child_ids(self, session_id: str) -> list[str]:
    """The ids of the direct children of *session_id*."""
    index = await self._get_index()
    return self._children_of(index, session_id)

  async def descendant_ids(self, session_id: str) -> list[str]:
    """The ids of every descendant of *session_id*, at any depth."""
    index = await self._get_index()
    return self._descendants(index, session_id)

  async def _note_run_liveness(self, session_id: str, run: models.RunRecord, launched: bool) -> None:
    """The run owner's liveness notification: a worker node's busy interval.

    A worker node's busy interval (its thinking_since, the header timer) opens
    at its Run's recorded started_at and closes at that Run's terminal fact
    (every exit path lands one through record_finish / dispatch.finish_run). A
    manager node's busy interval stays owned by the master queue, so a
    manager_turn Run opens nothing here and its finish closes nothing. The
    node's has_running_tasks and work_state are the task-tree activity
    derivation's (derive_task_tree_activity), never this notification's.
    Failures are logged by the run owner's seam and never fail the durable
    launch or finish that preceded them.
    """
    if not launched:
      thinking_state.clear_run_busy(session_id, run.id)
      return
    thinking_state.note_run_backend(session_id, run.backend)
    index = await self._get_index()
    meta = index.metas.get(session_id)
    if meta is not None and meta.profile == "worker":
      thinking_state.mark_run_busy(session_id, run.id, since=run.started_at)

  async def _require_open_ancestry(self, session_id: str) -> list[models.SessionMetadata]:
    index = await self._get_index()
    return await self._require_open_ancestry_from_index(index, session_id)

  # ------------------------------------------------------------------
  # Derived facts (rebuilt from durable facts, suffix-memoized)
  # ------------------------------------------------------------------

  def fact_history(self, session_id: str) -> list[dict]:
    """The session's full durable fact history: archived segments, then the live log.

    A close/import boundary, a successful acknowledgement, or a delivered
    report never disappears when chat_events.jsonl rotates — the archived
    segments remain part of the fact history every fold and every recovery
    scan reads.
    """
    live = self.session_events.load_chat_events_sync(session_id)
    archived_count = self._archived_event_count(session_id, live)
    if not archived_count:
      return live
    return [*self._load_archived_events(session_id, archived_count), *live]

  async def read_fact_history(self, session_id: str) -> list[dict]:
    """The full fact history, answered on the event loop when memory holds all of it.

    A warm events cache whose archive offset the metadata cache holds at zero is the whole history, so
    the loop returns it with no I/O. A cold cache or an archived session reads through fact_history on
    a worker thread.
    """
    live = self.session_events.chat_events.peek_cached_events(session_id)
    if live is not None and self.session_events.chat_events.peek_archive_offset(session_id) == 0:
      return live
    return await asyncio.to_thread(self.fact_history, session_id)

  def _archived_event_count(self, session_id: str, live: list[dict]) -> int:
    total = self.session_events.get_chat_event_count_sync(session_id)
    return max(0, total - len(live))

  def _load_archived_events(self, session_id: str, count: int) -> list[dict]:
    events, _has_more = self.session_events.load_chat_events_range(session_id, 0, count)
    return events

  def _history_identity(self, session_id: str) -> tuple[int, int, int] | None:
    """The live history file's (inode, size, mtime); None while the file does not exist.

    Every history write appends to the file or replaces it, so a memo keyed on this
    identity cannot outlive the write it describes.
    """
    try:
      st = os.stat(chat_events.chat_events_path(self._cfg.sessions_dir / session_id))
    except OSError:
      return None
    return (st.st_ino, st.st_size, st.st_mtime_ns)

  def _task_state_fast(self, session_id: str) -> str:
    """The task state read from the history's end, without loading the history.

    See :meth:`_task_state_at`; this reads the node's current history identity.
    """
    return self._task_state_at(session_id, self._history_identity(session_id))

  def _task_state_at(self, session_id: str, identity: tuple[int, int, int] | None) -> str:
    """The task state of the history at *identity*: its newest lifecycle fact, else open.

    The walk streams the live file backwards and then the archive segments, and
    stops at the first lifecycle fact (clone_start / task_closed / task_reopened).
    That is the verdict the full fold's last-wins pass gives, and nothing is
    cached but the verdict: a tail without a lifecycle fact costs a walk, not a
    history load. The memo is keyed by *identity*, so any lifecycle write moves
    the key and a stale verdict cannot survive a mutation.
    """
    memoized = self._memoized_task_state(session_id, identity)
    if memoized is not None:
      return memoized
    newest = next(
        self.session_events.chat_events.iter_events_from_end(
            session_id,
            _LIFECYCLE_STATE_TYPES,
            _live_end(identity),
            yield_gil_every=_TREE_WALK_GIL_YIELD_LINES,
        ), None)
    state = "open" if newest is None else _task_state_of_lifecycle(newest)
    self._state_tail_memo[session_id] = (identity, state)
    return state

  def _memoized_task_state(self, session_id: str, identity: tuple[int, int, int] | None) -> str | None:
    """The memoized task state at *identity*, or None when the walk has not run for it."""
    cached = self._state_tail_memo.get(session_id)
    if cached is not None and cached[0] == identity:
      return cached[1]
    return None

  def _run_facts_at(self, session_id: str, identity: tuple[int, int, int] | None) -> runs.RunFacts:
    """The node's folded Run facts at *identity*, read from the history without caching the history.

    The fold runs over the run facts alone, in forward order, so its last-wins
    rule matches :func:`runs.fold_run_facts`. The result is memoized by identity.
    """
    cached = self._outcomes_memo.get(session_id)
    if cached is not None and cached[0] == identity:
      return cached[1]
    newest_first = self.session_events.chat_events.iter_events_from_end(
        session_id, _RUN_FACT_TYPES, _live_end(identity), yield_gil_every=_TREE_WALK_GIL_YIELD_LINES)
    facts = runs.fold_run_facts(runs.RunFacts(), list(reversed(list(newest_first))))
    self._outcomes_memo[session_id] = (identity, facts)
    return facts

  def _facts_of(self, session_id: str) -> _TaskFacts:
    """The session's folded task/run facts over the full history (suffix-memoized).

    The memo rides the chat-events cache's list identity alone (append-only
    growth or wholesale replacement): token streaming extends the fold by its
    suffix, and a rotation re-keys the archived half because it rewrites the
    live file and drops the events cache, replacing the list. The archived
    extent rides that identity, so the warm path never re-derives it:
    archive_offset's only writer (the scheduled recycle) pairs the bump with
    the same rewrite and drop, so a warm path holding the list holds the
    extent, and the cell's stored count feeds the suffix fold. A
    task_imported fact in the suffix moves the input boundary, so the suffix
    fold that sees one restarts from the whole history.
    """
    live = self.session_events.load_chat_events_sync(session_id)
    cached = self._facts_memo.get(session_id)
    if cached is not None and cached[0] is live:
      facts = cached[2]
      archived_count = cached[1]
    else:
      archived_count = self._archived_event_count(session_id, live)
      facts = _TaskFacts()
      if archived_count:
        facts = _fold_task_events(facts, self._load_archived_events(session_id, archived_count), 0)
      facts.covered = 0
      self._facts_memo[session_id] = (live, archived_count, facts)
    suffix = live[facts.covered:]
    if suffix:
      if any(event.get("type") == ET.TASK_IMPORTED for event in suffix):
        rebuilt = _TaskFacts()
        if archived_count:
          rebuilt = _fold_task_events(rebuilt, self._load_archived_events(session_id, archived_count), 0)
        facts = _fold_task_events(rebuilt, live, archived_count)
      else:
        facts = _fold_task_events(facts, suffix, archived_count + facts.covered)
      facts.covered = len(live)
      self._facts_memo[session_id] = (live, archived_count, facts)
    return facts

  def facts_of(self, session_id: str) -> _TaskFacts:
    """Public fold entry for the input/completion owners (same memo)."""
    return self._facts_of(session_id)

  def task_state(self, session_id: str) -> str:
    """The task's derived lifecycle state."""
    return self._facts_of(session_id).task_state

  def task_state_fast(self, session_id: str) -> str:
    """The task's lifecycle state from the history tail (see _task_state_fast)."""
    return self._task_state_fast(session_id)

  def activity_of(self, session_id: str) -> TaskTreeActivity:
    """The node's derived sidebar activity: live-Run flag plus work verdict.

    The one derivation the sidebar's task-tree probe, the tree rows and the
    session detail share, so their verdicts for one node can never disagree.
    A caller reading several nodes of one projection holds a :class:`_NodeReads`
    and asks it instead, so the nodes read at one instant.
    """
    return _NodeReads(self).activity(session_id)

  def _activity_at(
      self,
      session_id: str,
      identity: tuple[int, int, int] | None,
      generation: int,
      task_open: Callable[[], bool],
  ) -> TaskTreeActivity:
    """The node's activity at one history identity and one records generation.

    Memoized on that pair. Every record mutation funnels through one
    :meth:`runs.RunStore.write_record` bump, and every history write moves the
    identity. A run record with ``ended_at`` is settled, so the Run facts are read
    only when some run is unsettled. A verdict that consulted /proc is never
    stored: a process death moves it with no file write to bump the key, so that
    node re-derives until its runs settle (the sidebar probe's recheck_liveness
    contract, unchanged).
    """
    memoized = self._memoized_activity(session_id, identity, generation)
    if memoized is not None:
      return memoized
    run_records = self.runs.list_run_records_sync(session_id)
    facts = runs.RunFacts()
    if any(run.ended_at is None for run in run_records):
      facts = self._run_facts_at(session_id, identity)
    probed = False

    def host_boot_time() -> datetime.datetime:
      nonlocal probed
      probed = True
      return self._host_boot_time()

    activity = derive_task_tree_activity(run_records, facts, host_boot_time, task_open)
    if not probed:
      self._activity_memo[session_id] = (generation, identity, activity)
    return activity

  def _memoized_activity(
      self, session_id: str, identity: tuple[int, int, int] | None, generation: int) -> TaskTreeActivity | None:
    """The memoized activity at *identity* and *generation*, or None when it must be derived."""
    cached = self._activity_memo.get(session_id)
    if cached is not None and cached[0] == generation and cached[1] == identity:
      return cached[2]
    return None

  def activity_pair_of(self, session_id: str) -> tuple[bool, str]:
    """``activity_of`` as the plain pair the sidebar snapshot stores.

    The pair form keeps :mod:`src.runtime.sidebar_state` (which must not import
    the tree owner) free to hold and compare the value.
    """
    activity = self.activity_of(session_id)
    return (activity.has_running_tasks, activity.work_state)

  def _host_boot_time(self) -> datetime.datetime:
    return runs.read_host_boot_time()

  async def derived_archived_ids(self) -> set[str]:
    """Task nodes the effective archive lists while their stored status stays active.

    The read-time overlay the listing block applies: a task node whose
    state is not open (the close fact is the archive), and any node inherited
    under one, list as archived with no status write. Reads the cached index;
    a node already archived by status needs no overlay and is left out.
    Subtree inheritance rides the same call: one shared memo keeps the pass
    linear in node count.
    """
    index = await self._get_index()
    memo: dict[str, bool] = {}
    reads = _NodeReads(self)
    return {
        session_id for session_id, meta in index.metas.items()
        if meta.status != models.SessionStatus.ARCHIVED and self._archived_of_pass(index, meta, memo, reads)
    }

  def archived_of(
      self,
      index: _TreeIndex,
      meta: models.SessionMetadata,
      memo: dict[str, bool] | None = None,
      reads: _NodeReads | None = None,
  ) -> bool:
    """Archive visibility with subtree inheritance (the effective archive's single owner).

    effective(n) is the node's own state-based value or its parent's inherited
    one: a node is archived exactly when its task state is not open (the close
    fact is the archive — completed and cancelled included). Every descendant
    reads its parent's effective value, so an archived ancestor archives the subtree at read time with no descendant metadata
    write. A restored node is open again, so it unarchives. A shared *memo*
    makes a pass over many nodes
    linear in node count (each chain resolves through already-computed
    ancestors); a relation cycle surfaces through the hop guard as
    TaskConflictError, never silently.
    """
    reads = _NodeReads(self) if reads is None else reads
    return self._effective_archived(index, meta.id, {} if memo is None else memo, reads)

  def _archived_of_pass(
      self, index: _TreeIndex, meta: models.SessionMetadata, memo: dict[str, bool], reads: _NodeReads) -> bool:
    """archived_of inside one caller-owned pass: the memo spans the caller's
    whole node walk, so chains already resolved serve the rest of the pass."""
    return self._effective_archived(index, meta.id, memo, reads)

  def _effective_archived(self, index: _TreeIndex, session_id: str, memo: dict[str, bool], reads: _NodeReads) -> bool:
    """One node's effective archive value with inheritance, memoized per pass.

    Walks up the task-parent chain to the first memoized node or a root, then
    folds the effective values back down; every node the walk touches lands in
    the memo, so a whole-listing pass never recomputes one. A node's own archive
    value is its task state from the read pass (the tail read, not the full fold),
    and an inherited True settles the subtree without any node's own read.
    Inheritance never overrides a node's own open state into visibility: an
    archived ancestor archives the subtree (and the invariant forbids the
    reverse shape — an open node under an archived ancestor).
    """
    chain: list[tuple[str, models.SessionMetadata]] = []
    seen: set[str] = set()
    current = session_id
    inherited = False
    while True:
      known = memo.get(current)
      if known is not None:
        inherited = known
        break
      if current in seen or len(chain) > task_errors.ANCESTOR_HOP_LIMIT:
        raise task_errors.TaskConflictError([f"task relation cycle through {current}"])
      seen.add(current)
      meta = self._index_meta(index, current)
      chain.append((current, meta))
      if meta.task_parent_id is None:
        inherited = False  # a root inherits nothing
        break
      current = meta.task_parent_id
    for sid, _meta in reversed(chain):
      if not inherited:
        # A node is archived exactly when its own task state is not open: the
        # close fact is the archive, and a completed child counts at once.
        inherited = reads.task_state(sid) != "open"
      memo[sid] = inherited
    return memo[session_id]

  def session_row(self, index: _TreeIndex, session_id: str, reads: _NodeReads | None = None) -> models.SessionRow:
    """One tree row; *reads* is the caller's read pass, so a page's rows agree with each other."""
    reads = _NodeReads(self) if reads is None else reads
    meta = self._index_meta(index, session_id)
    child_ids = self._children_of(index, session_id)
    descendants = self._descendants(index, session_id, reads)
    open_count = sum(1 for d in descendants if reads.task_state(d) == "open")
    running_count = sum(1 for d in descendants if reads.activity(d).work_state == "running")
    return models.SessionRow(
        id=meta.id,
        name=meta.name,
        profile=meta.profile,
        task_parent_id=meta.task_parent_id,
        task_state=reads.task_state(session_id),  # type: ignore[arg-type]
        work_state=reads.activity(session_id).work_state,
        archived=self.archived_of(index, meta, reads=reads),
        child_count=len(child_ids),
        open_descendant_count=open_count,
        running_descendant_count=running_count,
        has_unread=bool(meta.has_unread),
    )

  async def record_native_anchor(
      self,
      session_id: str,
      *,
      prompt_hash: str,
      backend: str,
      model: str | None,
      reset_anchor: bool,
  ) -> None:
    """Persist the native-context anchor provenance under the control lock.

    Called from the launch's spawn callback: the anchor is cleared only when
    this launch deliberately starts a fresh native context (a changed
    instruction hash, backend family, or model), and the identity fields always
    name the snapshot this conversation continues under. The three fields write
    through the authorized anchor channel
    (``SessionAnchors.persist_native_anchor_provenance``) — every authorized
    writer of ``native_backend`` is an anchor write, so a concurrent stale
    whole-object save cannot roll the identity back. A failed preparation never
    reaches this write, so the usable old anchor survives it.
    """
    async with self.control_lock:
      await self.session_anchors.persist_native_anchor_provenance(
          session_id, prompt_hash=prompt_hash, native_backend=backend, model=model)
      self._invalidate_index()  # any metadata write may move the projection inputs
      if reset_anchor:
        fresh = await self.session_store.read_metadata_fresh(session_id)
        if fresh is not None and fresh.cc_session_id is not None:
          # The fresh native context voids the old conversation anchor. The clear
          # goes through the authorized channel: a whole-object save's anchor
          # reconciliation would correct it back to the disk value and the reset
          # would silently do nothing.
          await self.session_anchors.clear_cc_session_anchor(session_id)

  def prompt_rule_summaries(self, meta: models.SessionMetadata, index: _TreeIndex, reads: _NodeReads) -> dict:
    """The scope/source/current-rule facts the Task/Context UI reads from the detail.

    Body content stays in the immutable store; this names each scope's ref,
    origin path, measured size, and how many descendants a subtree rule change
    would affect.
    """

    def summary(ref: str | None) -> dict:
      if ref is None:
        return {"ref": None, "source": None, "chars": 0, "text": None}
      path = self._prompt_bodies_dir / f"{ref}.md"
      body = path.read_text(encoding="utf-8")
      # The body rides the same read that measures it: the editor's existing
      # text comes from its authoritative source, never from a guess.
      return {"ref": ref, "source": str(path), "chars": len(body), "text": body}

    return {
        "subtree": summary(meta.subtree_prompt_ref),
        "node": summary(meta.node_prompt_ref),
        "affected_descendants": len(self._descendants(index, meta.id, reads)),
    }

  async def session_detail(self, session_id: str) -> dict:
    """The session detail projection: existing metadata plus the derived task fields.

    The derived fields are answered from the memos on the event loop when every
    verdict they need is memoized; otherwise they are derived on a worker thread.
    """
    await self.load_task_meta(session_id)
    index = await self._get_index()
    # Re-read through the index so the detail and the tree agree on one projection.
    indexed = index.metas.get(session_id)
    if indexed is None:
      raise task_errors.TaskNotFoundError(f"task {session_id} not found")
    try:
      derived = self._detail_fields(index, indexed, _NodeReads(self, warm_only=True))
    except _ColdReadError:
      derived = await asyncio.to_thread(self._detail_fields, index, indexed, _NodeReads(self))
    ancestors = self._ancestors(index, session_id)
    payload = indexed.model_dump(mode="json")
    payload.update({"ancestors": [models.AncestorRef(id=a.id, name=a.name).model_dump() for a in ancestors], **derived})
    return payload

  def _detail_fields(self, index: _TreeIndex, indexed: models.SessionMetadata, reads: _NodeReads) -> dict:
    """The derived task fields of one node, read through one pass."""
    return {
        "task_state": reads.task_state(indexed.id),
        "work_state": reads.activity(indexed.id).work_state,
        "archived": self.archived_of(index, indexed, reads=reads),
        "prompt_rules": self.prompt_rule_summaries(indexed, index, reads),
    }

  # ------------------------------------------------------------------
  # Create
  # ------------------------------------------------------------------

  async def adopt_metadata_slot(
      self,
      session_id: str,
      old: models.SessionMetadata,
      owner: str,
      *,
      fields: tuple[str, ...] | None = None,
  ) -> models.SessionMetadata:
    """Copy selected fields from *owner* on *old* onto this node."""
    async with self.control_lock:
      meta = await self.load_meta(session_id)
      if meta is None:
        raise task_errors.TaskNotFoundError(f"session {session_id} not found")
      metadata_slots.copy_fields(old, meta, owner, names=fields)
      meta.updated_at = models.utc_now()
      await self._save_meta(meta)
      return meta

  async def create_task(
      self,
      *,
      request_id: str,
      task_parent_id: str | None,
      profile: str,
      task: models.TaskSpec | None,
      name: str | None,
      backend: str | None,
      group: str | None = None,
      session_id: str | None = None,
      slot_values: dict[str, Any] | None = None,
      caller: object,
  ) -> models.SessionMetadata:
    """Create one task node; a replayed request returns the original product.

    The node id is (parent, request_id)-stable unless *session_id* names it:
    the summon and the operator's create bind a node to an id that exists
    before the node does, and only the operator and the server may name one.
    *slot_values* (keys a package registered on the session file) ride the same
    atomic publish as the metadata. Metadata
    plus the task_created fact are written into a temp directory and published
    with one rename, so a crash leaves either no node or a complete one.
    """
    if not request_id:
      raise task_errors.TaskInvalidError(TASK_CREATE_REQUEST_ID_REQUIRED)
    if profile not in ("manager", "worker"):
      raise task_errors.TaskInvalidError("profile must be 'manager' or 'worker'")
    if session_id is not None and _create_actor_for(caller) == control_events.ACTOR_AGENT:
      raise task_errors.TaskForbiddenError(AGENT_CREATE_SCOPE_REFUSAL)
    task_id = session_id or control_events.stable_task_id(task_parent_id, request_id)
    async with self.control_lock:
      existing = await self.load_meta(task_id)
      if existing is not None:
        # A replayed operation returns its original product only to a caller
        # authorized for that same create — the replay is never an
        # authorization bypass. The judgment reads the ORIGINAL product's
        # profile and task, never the replay's requested ones: re-labeling a
        # replayed request must not turn a worker create (a gated
        # implementation delegation) into an ungated manager create, nor
        # borrow the read-only verify exemption for a node created as
        # implementation.
        if isinstance(caller, run_token.CallerIdentity) and not caller.is_operator:
          parent_meta = await self.load_meta(task_parent_id) if task_parent_id is not None else None
          await self._authorize_agent_creation(caller, existing.profile, existing.task, task_parent_id, parent_meta)
        return existing
      parent_meta: models.SessionMetadata | None = None
      # Caller scope first: an agent's 403 must not depend on the target's shape.
      if isinstance(caller, run_token.CallerIdentity) and not caller.is_operator:
        if task_parent_id is not None:
          parent_meta = await self.load_meta(task_parent_id)
        await self._authorize_agent_creation(caller, profile, task, task_parent_id, parent_meta)
      if task_parent_id is not None:
        parent_meta = await self.load_meta(task_parent_id)
        if parent_meta is None:
          raise task_errors.TaskNotFoundError(f"task {task_parent_id} not found")
        if parent_meta.profile == "worker":
          raise task_errors.TaskInvalidError(f"parent task {task_parent_id} is not a manager")
        index = await self._get_index()
        parent_state = self.task_state(task_parent_id)
        if parent_state != "open":
          raise task_errors.TaskConflictError([f"parent task {task_parent_id} is {parent_state}"])
        await self._require_open_ancestry_from_index(index, task_parent_id)
      await self._publish_new_task(
          task_id=task_id,
          request_id=request_id,
          task_parent_id=task_parent_id,
          profile=profile,
          task=task,
          name=name,
          backend=backend,
          group=group,
          slot_values=slot_values or {},
          parent_meta=parent_meta,
          actor=_create_actor_for(caller),
      )
    self._invalidate_index()
    # The publish rename took the node out from under any cached entry.
    self.session_store.invalidate_cache(task_id)
    fresh = await self.load_meta(task_id)
    assert fresh is not None
    # The creation fact is durably published; connected clients learn about it
    # through the existing best-effort tree-notification seam. The signal goes
    # out AFTER the publication and the cache/index invalidation, so an
    # observer receiving it can immediately read the new node, its parent and
    # the refreshed ancestor counts. A notification failure is logged by the
    # sink and never fails the creation: the atomic task_created fact remains
    # the one durable home, and reconnect/reload recovers from it. A replayed
    # request (returned above) publishes nothing and signals nothing.
    await self.events.notify_tree_changed(task_id, ET.TASK_CREATED)
    return fresh

  async def _default_node_name(self, task: models.TaskSpec | None, profile: str) -> str:
    """A node created without a name takes the goal's first line when there is
    one; otherwise the session counter name ("Session N"), so the
    sidebar's one-click create names a task node the way it names a session.
    """
    if task is not None and task.goal.strip():
      return default_task_name(task, profile)
    return await self._next_session_name()

  async def _next_session_name(self) -> str:
    """Generate 'Session 0', 'Session 1', etc. using a persistent counter file.

    Reads the next number from sessions_dir/.counter (O(1) instead of listing
    all sessions). Falls back to counting directories when the counter is
    missing, unreadable, or unparsable.
    """
    counter_path = self._cfg.sessions_dir / ".counter"

    def _read_and_increment() -> int:
      self._cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
      # FileNotFoundError is an OSError, so a missing counter takes the same
      # count-dirs fallback as an unreadable or unparsable one; an exists()
      # pre-check would only open a check-then-read race.
      try:
        n = int(counter_path.read_text().strip())
      except (ValueError, OSError):
        n = self._count_session_dirs()
      counter_path.write_text(str(n + 1))
      return n

    n = await asyncio.to_thread(_read_and_increment)
    return f"Session {n}"

  def _count_session_dirs(self) -> int:
    """Count existing session directories for backward-compat counter init."""
    if not self._cfg.sessions_dir.exists():
      return 0
    return sum(1 for d in self._cfg.sessions_dir.iterdir() if d.is_dir())

  async def _authorize_agent_creation(
      self,
      caller: object,
      profile: str,
      task: models.TaskSpec | None,
      task_parent_id: str | None,
      parent_meta: models.SessionMetadata | None,
  ) -> None:
    """An agent caller may organize only its own open manager task.

    A logical manager child directly under the caller's own manager task is
    coordination work and needs no user authorization; a worker child is
    implementation delegation and still rides the runtime gate (takeoff_gate:
    a real user message or a thread platform summon somewhere on the open task
    chain) — except a read-only delegation (verify, cold-read), which the one
    shared judgment (takeoff_gate.is_read_only_exempt) excuses here exactly as
    it does on the route and at the launch. On a replayed create *task* is the
    ORIGINAL node's spec, so re-labeling a replay cannot borrow the exemption
    for a node created as implementation. Any other shape — an unrelated root,
    a foreign parent, a worker parent — is outside an agent's scope.
    """
    assert isinstance(caller, run_token.CallerIdentity)
    claims = caller.claims
    assert claims is not None
    if (task_parent_id != claims.session_id or parent_meta is None or parent_meta.profile != "manager"):
      raise task_errors.TaskForbiddenError(AGENT_CREATE_SCOPE_REFUSAL)
    if profile == "worker" and not takeoff_gate.is_read_only_exempt(task):
      # Implementation authorization stays with the caller's own manager task:
      # the runtime gate (takeoff_gate) decides. A read-only delegation
      # (verify, cold-read) needs no user message on the chain, on the same
      # task-type judgment the route and the launch apply.
      await self.check_task_authorization(claims.session_id)

  async def _require_open_ancestry_from_index(self, index: _TreeIndex, session_id: str) -> list[models.SessionMetadata]:
    """Every ancestor of *session_id* must be an open task (API: 409 otherwise)."""
    chain = self._ancestors(index, session_id)
    closed = [a.id for a in chain if self._facts_of(a.id).task_state != "open"]
    if closed:
      raise task_errors.TaskConflictError([closed_ancestors_blocker(closed)])
    return chain

  async def _publish_new_task(
      self,
      *,
      task_id: str,
      request_id: str,
      task_parent_id: str | None,
      profile: str,
      task: models.TaskSpec | None,
      name: str | None,
      backend: str | None,
      group: str | None,
      slot_values: dict[str, Any],
      parent_meta: models.SessionMetadata | None,
      actor: str,
  ) -> models.SessionMetadata:
    sessions_dir = self._cfg.sessions_dir
    sessions_dir.mkdir(parents=True, exist_ok=True)
    final_dir = sessions_dir / task_id
    temp_dir = sessions_dir / f".task-{task_id}-{os.getpid()}-{uuid.uuid4().hex}.tmp"
    meta = models.SessionMetadata(
        id=task_id,
        name=name or await self._default_node_name(task, profile),
        schema_version=2,
        profile=profile,  # type: ignore[arg-type]
        task=task,
        task_parent_id=task_parent_id,
        backend=backend or (parent_meta.backend if parent_meta else "") or self._cfg.backends.options[0].id,
        group=group,
    )
    metadata_slots.set_registered(meta, slot_values)
    try:
      (temp_dir / runs.DATA_DIR_NAME).mkdir(parents=True)
      # The creation fact is written into the temp node itself, so metadata and
      # the event log publish together with the one rename; post-publication
      # facts go through the sink.
      created_event = control_events.build_task_created_event(
          actor=actor,
          task_id=task_id,
          request_id=request_id,
          task_parent_id=task_parent_id,
          task_spec_hash=canonical_task_spec_hash(task),
      )
      await ndjson.append_ndjson(chat_events.chat_events_path(temp_dir), created_event)
      meta.created_by_event = models.EventRef(session_id=task_id, event_id=str(created_event["id"]))
      await asyncio.to_thread(
          json_utils.atomic_write_text, temp_dir / runs.METADATA_NAME,
          meta.model_dump_json(indent=2, exclude=session_store.TRANSIENT_METADATA_FIELDS))
      try:
        os.replace(temp_dir, final_dir)
      except OSError:
        # Lost a create race for the same stable id: the original product exists.
        existing = self._read_metadata_file(final_dir / runs.METADATA_NAME)
        if existing is None:
          raise
        return existing
      return meta
    finally:
      if temp_dir.exists():
        shutil.rmtree(temp_dir, ignore_errors=True)

  @staticmethod
  def _read_metadata_file(path) -> models.SessionMetadata | None:
    if not path.exists():
      return None
    return models.validate_session_metadata(path.read_text(encoding="utf-8"), str(path))

  async def check_task_authorization(self, session_id: str) -> str:
    """The runtime gate for a v2 task caller (takeoff_gate).

    The calling node must be a manager, every node up to the root an open
    task, and some node on that chain must hold a real user message or a
    thread platform summon; other agent messages, scheduled triggers and child
    reports never count.
    """
    index = await self._get_index()

    def meta_of(sid: str) -> tuple[str | None, str | None]:
      meta = index.metas.get(sid)
      if meta is None:
        return None, None
      return meta.task_parent_id, meta.profile

    def state_of(sid: str) -> str:
      meta = index.metas.get(sid)
      if meta is None:
        return "open"
      return self.task_state(sid)

    return takeoff_gate.check_takeoff_gate_for_task(
        session_id,
        load_events=self.events.load_events,
        task_meta_of=meta_of,
        task_state_of=state_of,
    )

  async def update_slot_fields(self, session_id: str, owner: str, **values: Any) -> models.SessionMetadata:
    """Write registered metadata-slot fields without refreshing the sidebar sort key.

    The node is re-read under the control lock, so a concurrent task edit
    between the caller's earlier read and this write is preserved. Saving
    directly leaves ``updated_at`` in place while still dirtying the sidebar,
    bumping the listing revision and invalidating the tree index.
    """
    if not values:
      raise task_errors.TaskInvalidError("update_slot_fields requires at least one field")
    async with self.control_lock:
      meta = await self.load_meta(session_id)
      if meta is None:
        raise task_errors.TaskNotFoundError(f"session {session_id} not found")
      metadata_slots.set_fields(meta, owner, **values)
      await self.session_store.save_metadata(meta)
      self._invalidate_index()
      return meta

  async def set_node_backend(self, session_id: str, backend: str) -> None:
    """Point a worker node at *backend*; its later Runs and messages resolve the backend from the node.

    The write runs under the control lock and leaves the sidebar sort key alone, as update_slot_fields does.
    """
    async with self.control_lock:
      meta = await self.load_meta(session_id)
      if meta is None:
        raise task_errors.TaskNotFoundError(f"session {session_id} not found")
      meta.backend = backend
      await self.session_store.save_metadata(meta)
      self._invalidate_index()

  async def create_retry(
      self,
      session_id: str,
      request_id: str,
      original_run_id: str,
      *,
      backend: str | None = None,
      input_event_ids: list[str] | None = None,
  ) -> dict:
    """Create one retry run on the open task; a replayed request returns the same run.

    The retry pins the task's current spec text (later edits keep the old
    versions pinned to their own runs) and inherits the retried run's
    execution context; the previous run's evidence is preserved untouched.
    *backend* moves the retry to that backends.options id (its default model);
    *input_event_ids* binds the retry to that input batch, as a manager-round
    retry binds its original's. Both are None for a manual worker retry.
    """
    async with self.control_lock:
      await self._get_index()
      meta = await self.load_task_meta(session_id)
      state = self.task_state(session_id)
      if state != "open":
        raise task_errors.TaskConflictError([f"task {session_id} is {state}; only open tasks accept retries"])
      original = await self.runs.get_run(session_id, original_run_id)
      if original is None:
        raise task_errors.TaskNotFoundError(f"run {original_run_id} not found in session {session_id}")
      retry_fields: dict = {}
      if original.kind == "manager_turn":
        # A manager-round retry reruns that round's own batch: the retry Run
        # binds the original's input_event_ids, so the launch path skips the
        # pending claim (the original batch counts as handled) and the finish
        # payload is exactly that batch. Worker retries stay claimless — they
        # consume whatever is pending when they launch.
        retry_fields["input_event_ids"] = list(original.input_event_ids)
      if input_event_ids is not None:
        retry_fields["input_event_ids"] = list(input_event_ids)
      backend_id, model = original.backend, original.model
      if backend is not None:
        option = config.require_backend_option(self._cfg, backend, subject="retry ")
        backend_id, model = option.id, models.option_default_model(option, subject="retry ")
      run = await self.runs.create_retry_run_locked(
          session_id,
          request_id,
          original_run_id,
          task_spec_text=canonical_task_spec_text(meta.task),
          kind=original.kind,
          backend=backend_id,
          model=model,
          repo_path=original.repo_path,
          base_branch=original.base_branch,
          branch_name=original.branch_name,
          worktree_path=original.worktree_path,
          sequence_ref=original.sequence_ref,
          # A review retry stays chained to the same work Run (the work/spec/
          # review pin must survive the retry).
          review_of_run_id=original.review_of_run_id,
          **retry_fields,
      )
    return {"session_id": session_id, "run_id": run.id}

  # ------------------------------------------------------------------
  # Patch
  # ------------------------------------------------------------------

  async def patch_task(
      self, session_id: str, req: models.PatchSessionTaskRequest, *, caller: object) -> models.SessionMetadata:
    """Apply one v2 metadata mutation with its structural guards (operator-only)."""
    task_errors.require_operator(caller, "task metadata mutations require operator credentials")
    async with self.control_lock:
      index = await self._get_index()
      meta = await self.load_task_meta(session_id)
      fs = req.model_fields_set
      structural = bool(fs & {"task", "profile", "task_parent_id"})
      blockers = self._structural_blockers(session_id) if structural else []
      if ("profile" in fs and req.profile is not None and req.profile != meta.profile and req.profile == "worker" and
          self._children_count(index, session_id) > 0):
        blockers.append("demotion to worker requires a task with no child tasks")
      if "task_parent_id" in fs and req.task_parent_id != meta.task_parent_id:
        blockers.extend(await self._reparent_blockers(session_id, req.task_parent_id))
      if blockers:
        raise task_errors.TaskConflictError(sorted(set(blockers)))
      if "name" in fs and req.name is not None:
        meta.name = req.name
      if "profile" in fs and req.profile is not None:
        meta.profile = req.profile
      if "task" in fs:
        meta.task = req.task
      if "task_parent_id" in fs:
        meta.task_parent_id = req.task_parent_id
      for scope in ("subtree", "node"):
        field = f"{scope}_prompt"
        if field in fs:
          await self._apply_prompt_change(session_id, meta, scope, getattr(req, field))
      meta.schema_version = 2
      await self._save_meta(meta)
      await self.events.notify_tree_changed(
          session_id, ET.PROMPT_CHANGED if fs & {"subtree_prompt", "node_prompt"} else "task_updated")
      return meta

  def _children_count(self, index: _TreeIndex, session_id: str) -> int:
    return len(self._children_of(index, session_id))

  def _structural_blockers(self, session_id: str) -> list[str]:
    """Running and pending-execution blockers of one node (run facts; input seam extends)."""
    blockers: list[str] = []
    if self.pending_input_blockers is not None:
      blockers.extend(self.pending_input_blockers(session_id))
    run_records = self.runs.list_run_records_sync(session_id)
    if not run_records:
      return blockers
    events = self.runs.load_events_sync(session_id)
    host_boot = self._host_boot_time()
    for run in run_records:
      blocker = self.runs.run_blocker(run, events, host_boot)
      if blocker is not None:
        blockers.append(blocker)
    return blockers

  async def _reparent_blockers(self, session_id: str, new_parent_id: str | None) -> list[str]:
    """Reparent validation: open manager/root target, no cycles, no running subtree."""
    index = await self._get_index()
    self._index_meta(index, session_id)
    blockers: list[str] = []
    if new_parent_id is not None:
      target = self._index_meta(index, new_parent_id)
      if target.profile != "manager":
        blockers.append(f"reparent target {new_parent_id} is not a manager task")
      if self.task_state(new_parent_id) != "open":
        blockers.append(f"reparent target {new_parent_id} is {self.task_state(new_parent_id)}")
      chain_ids = [a.id for a in self._ancestors(index, new_parent_id)]
      if session_id == new_parent_id or session_id in chain_ids:
        blockers.append(f"reparent target {new_parent_id} is inside {session_id}'s own subtree")
      else:
        closed = [a.id for a in self._ancestors(index, new_parent_id) if self._facts_of(a.id).task_state != "open"]
        if closed:
          blockers.append(closed_ancestors_blocker(closed))
    # The moving subtree itself must be idle: every node in it, self included.
    subtree = [session_id, *self._descendants(index, session_id)]
    for sid in subtree:
      blockers.extend(f"{sid}: {b}" for b in self._structural_blockers(sid))
      # Every undelivered close report anywhere in the moving subtree is
      # repaired before a move; historical report_to recipients are fixed
      # facts and are never rewritten by the move itself.
      blockers.extend(f"{sid}: {b}" for b in self.dispatch.undelivered_report_blockers(sid))
    return blockers

  async def _apply_prompt_change(
      self, session_id: str, meta: models.SessionMetadata, scope: str, body: str | None) -> None:
    """Store the rule body immutable, durably swap the reference, land the fact.

    The metadata write and the ``prompt_changed`` fact are two durable writes,
    and a crash between them is repaired without a workflow state machine: the
    fact is derived by comparing the metadata's current ref with the last
    recorded fact for that scope. Before extending the chain, an interrupted
    earlier edit is landed first (the metadata is ahead of its fact stream), so
    a retry or a later different edit can never fold two edits into one fact or
    record a wrong transition. Immutable body storage and the metadata owner
    stay exactly as they were; body writes are content-addressed, so a retried
    PATCH re-stores the same bytes and references the same ref.
    """
    await self._ensure_prompt_changed_fact(session_id, meta, scope)
    attr = f"{scope}_prompt_ref"
    previous_ref: str | None = getattr(meta, attr)
    new_ref: str | None = None
    if body is not None:
      new_ref = await asyncio.to_thread(self._store_prompt_body, body)
    if new_ref == previous_ref:
      return
    setattr(meta, attr, new_ref)
    await self._save_meta(meta)
    await self._ensure_prompt_changed_fact(session_id, meta, scope)

  async def _ensure_prompt_changed_fact(self, session_id: str, meta: models.SessionMetadata, scope: str) -> None:
    """Land the one ``prompt_changed`` fact the metadata's current ref implies.

    Idempotent across the crash boundary: a landed fact (the last event's
    ``new_ref`` equals the metadata ref) appends nothing; an interrupted edit
    (metadata saved, fact lost) appends exactly the missing transition, with
    ``previous_ref`` taken from the last fact so the chain stays truthful.
    """
    events = self.events.load_events(session_id)
    last = next((e for e in reversed(events) if e.get("type") == ET.PROMPT_CHANGED and e.get("scope") == scope), None)
    current: str | None = getattr(meta, f"{scope}_prompt_ref")
    if last is not None and last.get("new_ref") == current:
      return
    if last is None and current is None:
      return  # no rule was ever set on this scope: nothing to land
    await self.events.append(
        session_id,
        control_events.build_control_event(
            ET.PROMPT_CHANGED,
            actor=control_events.ACTOR_USER,
            source_session_id=session_id,
            scope=scope,
            previous_ref=last.get("new_ref") if last is not None else None,
            new_ref=current,
        ))

  def _store_prompt_body(self, body: str) -> str:
    """Write the rule body once under its SHA-256 fingerprint; returns the fingerprint ref."""
    ref = control_events.sha256_hex(body)
    path = self._prompt_bodies_dir / f"{ref}.md"
    if path.exists():
      existing = path.read_text(encoding="utf-8")
      if existing != body:
        raise RuntimeError(f"prompt body store corrupted at {path}: fingerprint content mismatch")
      return ref
    path.parent.mkdir(parents=True, exist_ok=True)
    json_utils.atomic_write_text(path, body)
    return ref

  # ------------------------------------------------------------------
  # Tree queries
  # ------------------------------------------------------------------

  def _has_running_work_descendant(self, index: _TreeIndex, session_id: str, reads: _NodeReads) -> bool:
    """True when any descendant's current work is running."""
    return any(reads.activity(d).work_state == "running" for d in self._descendants(index, session_id, reads))

  def _tree_rows_sync(self, index: _TreeIndex, parent_id: str | None,
                      include_archived: bool) -> list[models.SessionRow]:
    """The rows of one parent's children, computed from one read pass over the history.

    Runs on a worker thread: every read here is of the index (immutable once
    built) or of the node's files, and the only memos it writes are keyed by
    history identity or records generation (see _NodeReads).
    """
    reads = _NodeReads(self)
    rows_all = []
    for sid in self._children_of(index, parent_id):
      rows_all.append(self.session_row(index, sid, reads))
      # Release the GIL between rows: most rows are sub-millisecond, and the loop waits on the GIL between them.
      time.sleep(0)
    if not include_archived:
      # An archived row stays navigable while running work lives at or below
      # it: dropping it would sever the path to that work in a partial client
      # tree. The row's OWN work state counts — under inheritance a running
      # leaf below a retained archived parent is itself archived, and only its
      # own state keeps it (and with it the parent's child page) visible. The
      # row still reports archived=true.
      rows_all = [
          r for r in rows_all
          if not r.archived or r.work_state == "running" or self._has_running_work_descendant(index, r.id, reads)
      ]
    return rows_all

  async def tree_page(
      self,
      *,
      parent_id: str | None,
      include_archived: bool,
      limit: int,
      cursor: str | None,
  ) -> dict:
    """One revision-bound keyset page of the task tree; stale pagination is a 409.

    The index is read on the event loop; the rows are computed on a worker thread,
    so the loop keeps answering other requests while the page's histories are read.
    """
    index = await self._get_index()
    if parent_id:
      self._index_meta(index, parent_id)
    async with self._tree_rows_lock:
      rows_all = await asyncio.to_thread(self._tree_rows_sync, index, parent_id or None, include_archived)
    rows_all.sort(key=lambda r: (index.metas[r.id].created_at, r.id))
    after = _decode_tree_cursor(cursor) if cursor else None
    if after is not None:
      after_revision, after_key = after
      if after_revision != index.revision:
        raise task_errors.TaskConflictError(["task tree changed during pagination; refresh and re-paginate"])
      rows_all = [r for r in rows_all if (index.metas[r.id].created_at, r.id) > after_key]
    page = rows_all[:limit]
    next_cursor = None
    if len(rows_all) > limit and page:
      last = page[-1]
      next_cursor = _encode_tree_cursor(index.revision, (index.metas[last.id].created_at, last.id))
    return {"items": [jsonable_row(r) for r in page], "next_cursor": next_cursor, "tree_revision": index.revision}

  # ------------------------------------------------------------------
  # Deletion checks
  # ------------------------------------------------------------------

  def _deletion_reference_blockers(self, index: _TreeIndex, session_id: str) -> list[str]:
    """Saved child, run, and trigger references that prevent deletion."""
    blockers: list[str] = []
    children = self._children_of(index, session_id)
    if children:
      blockers.append(f"has child task(s): {', '.join(children)}")
    run_records = self.runs.list_run_records_sync(session_id)
    if run_records:
      blockers.append(f"has run record(s): {', '.join(r.id for r in run_records)}")
    triggers_dir = self._cfg.sessions_dir / session_id / "triggers"
    if triggers_dir.is_dir() and any(triggers_dir.glob("*.json")):
      blockers.append("has saved trigger reference(s)")
    return blockers

  def _deletion_blockers_locked(self, index: _TreeIndex, session_id: str) -> list[str]:
    """The v2 empty/unreferenced rule, evaluated under the control lock.

    No children, runs, triggers, or any other saved structured reference
    (origin/created-by/parent/successor pointers from other records, child
    reports another log holds), and no preserved conversation or evidence
    beyond the creation fact itself.
    """
    blockers = self._deletion_reference_blockers(index, session_id)
    facts = self._facts_of(session_id)
    substance = [e for e in facts.events_by_id.values() if e.get("type") != ET.TASK_CREATED]
    if substance:
      kinds = sorted({str(e.get("type")) for e in substance})
      blockers.append(f"has preserved conversation/evidence: {', '.join(kinds)}")
    for other_id, other in index.metas.items():
      if other_id == session_id:
        continue
      refs: list[str] = []
      if other.parent_session_id == session_id:
        refs.append("parent_session_id")
      if other.successor_session_id == session_id:
        refs.append("successor_session_id")
      if other.origin_ref is not None and other.origin_ref.session_id == session_id:
        refs.append("origin_ref")
      if other.created_by_event is not None and other.created_by_event.session_id == session_id:
        refs.append("created_by_event")
      if refs:
        blockers.append(f"referenced by task {other_id} ({', '.join(refs)})")
        continue
      other_facts = self._facts_of(other_id)
      if any(child == session_id for child, _event in other_facts.delivered_reports):
        blockers.append(f"referenced by a child report in task {other_id}")
    return blockers

  async def delete_permanently(self, session_id: str, *, caller: object) -> bool:
    """Check and delete under the one control lock: no separate toctou window.

    Operator scope only for v2 nodes; the empty/unreferenced rule is
    re-evaluated inside the lock immediately before the delete.
    """
    task_errors.require_operator(caller, "permanent delete requires operator credentials")
    async with self.control_lock:
      index = await self._get_index(force=True)
      self._index_meta(index, session_id)
      blockers = self._deletion_blockers_locked(index, session_id)
      if blockers:
        raise task_errors.TaskConflictError(sorted(set(blockers)))
      result = await self._lifecycle.delete_session_permanently(session_id)
    if result:
      self._invalidate_index()
      self._facts_memo.pop(session_id, None)
      self._outcomes_memo.pop(session_id, None)
      self._activity_memo.pop(session_id, None)
    return result

  async def clone_blocker(self, source_id: str) -> str | None:
    """The clone refusal for one source, or None: a manager source's parent
    task must be open — a clone writes nothing under a closed parent."""
    source = await self.load_meta(source_id)
    if source is None or source.profile != "manager" or source.task_parent_id is None:
      return None
    state = self.task_state(source.task_parent_id)
    if state != "open":
      return f"parent task {source.task_parent_id} is {state}; the clone writes nothing"
    return None

  async def run_elone_replacement(
      self,
      source: models.SessionMetadata,
      publish: Callable[[], Awaitable[models.SessionMetadata]],
      archive: Callable[[str], Awaitable[None]],
  ) -> models.SessionMetadata:
    """Replace one manager source with its elone child; returns the child.

    The whole replacement runs under the control lock, in this order: the
    refusals (an unfinished run on the source, a closed parent task), the
    subtree's undelivered close reports, *publish* (the child's birth), the
    child-edge move, the source's archive-format close, *archive* (the stored
    status and successor pointer), and the scheduled-task rebind. A failed
    step raises with the steps that completed listed.
    """
    completed: list[str] = []
    async with self.control_lock:
      try:
        blockers = self._elone_blockers(source)
        if blockers:
          raise task_errors.TaskConflictError(blockers)
        completed.append("validated the source")
        await self._redeliver_subtree_close_reports(source.id)
        completed.append("redelivered the subtree's undelivered close reports")
        new_meta = await publish()
        completed.append("published the new node")
        await self._move_source_children(source.id, new_meta.id)
        completed.append("moved the source's child tasks")
        await self._close_source_archived(source.id, new_meta.id)
        completed.append("closed the source")
        await archive(new_meta.id)
        completed.append("archived the source with its successor pointer")
        await self._rebind_scheduled_tasks(source, new_meta)
        completed.append("rebound the scheduled tasks")
      except Exception as e:
        e.add_note(f"elone of {source.id} completed before the failure: {', '.join(completed) or 'no step'}")
        raise
    return new_meta

  def _elone_blockers(self, source: models.SessionMetadata) -> list[str]:
    """The elone refusals: an unfinished run on the source, a closed parent task."""
    blockers: list[str] = []
    events = self.runs.load_events_sync(source.id)
    host_boot = self._host_boot_time()
    for run in self.runs.list_run_records_sync(source.id):
      blocker = self.runs.run_blocker(run, events, host_boot)
      if blocker is not None:
        blockers.append(f"{source.id}: {blocker}")
    if source.task_parent_id is not None:
      state = self.task_state(source.task_parent_id)
      if state != "open":
        blockers.append(f"parent task {source.task_parent_id} is {state}")
    return blockers

  async def _redeliver_subtree_close_reports(self, source_id: str) -> None:
    """Land every close report the moving subtree still owes its recipient.

    The source is the dominant recipient (its direct children's close facts
    name it), so the redelivery runs before the source closes and no close
    report arrives at a closed node. Deliveries wake nobody: the locked
    variant's caller owns the wakes, and the source closes right after.
    """
    index = await self._get_index(force=True)
    for descendant_id in self._descendants(index, source_id):
      await self.dispatch.recover_pending_reports_locked(descendant_id)

  async def _move_source_children(self, source_id: str, new_id: str) -> None:
    """Point every direct child's decomposition edge at the new node.

    The edge is the only move: the children keep their own logs, close facts
    and report recipients, and the subtree move's idle check does not apply —
    the position stays where it was, so a running child moves and keeps
    reporting, its next report reading the new edge.
    """
    index = await self._get_index(force=True)
    for child_id in self._children_of(index, source_id):
      child = await self.load_task_meta(child_id)
      child.task_parent_id = new_id
      await self._save_meta(child)

  async def _close_source_archived(self, source_id: str, new_id: str) -> None:
    """Close an open source with the archive format; a closed source keeps its fact."""
    await self.completion.archive_replaced_source_locked(source_id, new_id)

  async def _rebind_scheduled_tasks(self, source: models.SessionMetadata, new_meta: models.SessionMetadata) -> None:
    """Move every scheduled task bound to the source onto the new node.

    Each registered sequence controller rebinds its own bindings' yaml
    ``session_id``; the scheduler's bookkeeping copies to the new node, so the
    next fire computes its due time from the source's true last occurrence and
    lands on the child.
    """
    moved = False
    for controller in sequence_controllers.sequence_controllers():
      if await controller.move_bindings(source.id, new_meta.id):
        moved = True
    if not moved:
      return
    fresh_new = await self.load_task_meta(new_meta.id)
    metadata_slots.copy_fields(
        source, fresh_new, "cron", names=("last_scheduled_run", "last_scheduled_cron", "last_run_status"))
    await self._save_meta(fresh_new)


# ---------------------------------------------------------------------------
# Module helpers
# ---------------------------------------------------------------------------


def _create_actor_for(caller: object) -> str:
  """The creation fact's actor for one create_task caller.

  A verified operator is a user; the configured scheduler's server-owned fire
  passes the "system" sentinel (provenance the server owns — never a payload
  bit a run-token caller can forge); anything else is an agent.
  """
  if isinstance(caller, run_token.CallerIdentity) and caller.is_operator:
    return control_events.ACTOR_USER
  if caller == "system":
    return control_events.ACTOR_SYSTEM
  return control_events.ACTOR_AGENT


def canonical_task_spec_text(task: models.TaskSpec | None) -> str | None:
  """The canonical serialized task-spec body a run pins (and hashes) at launch."""
  if task is None:
    return None
  return orjson.dumps(task.model_dump()).decode()


def canonical_task_spec_hash(task: models.TaskSpec | None) -> str | None:
  """The SHA-256 fingerprint of the canonical task-spec body (None without a spec)."""
  text = canonical_task_spec_text(task)
  return control_events.sha256_hex(text) if text is not None else None


@dataclasses.dataclass
class _TreeIndex:
  """The rebuildable parent/child index over one sessions directory."""
  metas: dict[str, models.SessionMetadata]
  children: dict[str | None, list[str]]
  revision: str
  root_sig: tuple[int, int]


# A Markdown ATX heading: up to three leading spaces, then 1-6 '#' closed by
# whitespace or end of line (so "####### tag" is content, not a heading).
_MD_HEADING_RE = re.compile(r"^\s{0,3}#{1,6}(\s|$)")


def default_task_name(task: models.TaskSpec | None, profile: str) -> str:
  """The goal's first content line, skipping Markdown headings because delegation goals open with a "## Goal" heading; else "New <profile> task"."""
  if task is not None and task.goal.strip():
    nonempty = [line for line in task.goal.splitlines() if line.strip()]
    content = [line for line in nonempty if not _MD_HEADING_RE.match(line)]
    if content:
      return content[0].strip()[:80]
    heading_text = nonempty[0].lstrip().lstrip("#").strip()
    if heading_text:
      return heading_text[:80]
  return f"New {profile} task"


def jsonable_row(row: models.SessionRow) -> dict:
  return row.model_dump()


def _encode_tree_cursor(revision: str, key: tuple[datetime.datetime, str]) -> str:
  """Revision-bound keyset cursor: base64url JSON of (revision, created_at, id)."""
  payload = {"r": revision, "a": key[0].isoformat(), "i": key[1]}
  return run_token.b64url_encode(orjson.dumps(payload))


def _decode_tree_cursor(cursor: str) -> tuple[str, tuple[datetime.datetime, str]]:
  """Decode one tree cursor; a malformed value fails loud (API: 400)."""
  try:
    payload = orjson.loads(run_token.b64url_decode(cursor))
    key = (models.ensure_utc(datetime.datetime.fromisoformat(payload["a"])), payload["i"])
    return payload["r"], key
  except (ValueError, KeyError, TypeError) as e:
    raise task_errors.TaskInvalidError(f"malformed tree page cursor: {cursor!r}") from e
