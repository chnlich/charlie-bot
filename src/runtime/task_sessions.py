"""Task-tree owner: task metadata mutations, tree validation, and derived queries.

This module is the single owner of task metadata, of the task-tree
relation (``task_parent_id`` edges over flat ``sessions/<id>`` directories),
and of the derived task projection (``task_state`` / ``work_state`` / archive
visibility / subtree counts). The legacy conversation, attachment, rating and
page-aggregation services stay in :mod:`src.runtime.sessions`.

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

import asyncio
import os
import re
import shutil
import time
import uuid
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

import orjson

from src.infra import event_types as ET
from src.infra import metadata_slots
from src.infra.config import CharlieBotConfig
from src.infra.event_types import is_real_user_message
from src.infra.json_utils import atomic_write_text
from src.infra.models import (
    AncestorRef,
    EventRef,
    PatchSessionTaskRequest,
    SessionMetadata,
    SessionRow,
    SessionStatus,
    TaskSpec,
    WorkState,
    ensure_utc,
    utc_now,
    validate_session_metadata,
)
from src.infra.ndjson import append_ndjson
from src.infra.tasks import create_logged_task
from src.runtime import control_sink
from src.runtime.chat_events import chat_events_path
from src.runtime.control_events import (
    ACTOR_AGENT,
    ACTOR_SYSTEM,
    ACTOR_USER,
    build_control_event,
    build_task_created_event,
    sha256_hex,
    stable_close_event_id,
    stable_task_id,
)
from src.runtime.run_token import CallerIdentity, b64url_decode, b64url_encode
from src.runtime.runs import DATA_DIR_NAME, METADATA_NAME, RunStore, is_run_alive, stop_requested_in_events
from src.runtime.session_dispatch import INPUT_EVENT_TYPES, TaskInputDispatcher
from src.runtime.session_store import TRANSIENT_METADATA_FIELDS
from src.runtime.sessions import SessionManager
from src.runtime.takeoff_gate import is_verify_exempt
from src.runtime.task_completion import TaskCompletionManager
from src.runtime.task_errors import (
    ANCESTOR_HOP_LIMIT,
    TaskConflictError,
    TaskForbiddenError,
    TaskInvalidError,
    TaskNotFoundError,
    require_operator,
)
from src.runtime.thinking_state import clear_run_busy, mark_run_busy, note_run_backend

if TYPE_CHECKING:
  from src.infra.models import RunRecord

PROMPT_BODIES_DIR_NAME = "prompt_bodies"

# The rebuildable index self-heals on this cadence even when nothing the owner
# wrote moved the sessions root: one metadata walk per TTL per process is the
# bound an out-of-band edit (none exists in the single-service model) could
# stay invisible for.
_TREE_INDEX_TTL_SECONDS = 2.0

# The create route (src/runtime/api/sessions.py) reproduces these two refusals
# verbatim as its client-visible details — the first in the v2 pre-check, the
# second in the legacy-shape guard; the wording lives beside the raises that
# own the contracts.
TASK_CREATE_REQUEST_ID_REQUIRED = "request_id is required for task creation"
AGENT_CREATE_SCOPE_REFUSAL = "an agent may only create a task directly under its own open manager task"


def closed_ancestors_blocker(closed_ids: list[str]) -> str:
  """The 409 blocker sentence naming a node's closed ancestor tasks."""
  return f"closed ancestor task(s): {', '.join(closed_ids)}"


@dataclass
class _TaskFacts:
  """The derived facts one session's full event history folds to.

  The fold is the single derived-fact owner for the tree projection, the
  input dispatcher, and the completion owner: task lifecycle, run outcomes,
  input candidacy with its boundary, close and close-request facts, and
  delivered child reports all come from this one pass over the durable
  events (archived segments included).
  """
  task_state: str = "open"
  run_outcomes: dict[str, str] = field(default_factory=dict)
  # Each run's first run_finished fact's absolute log position (the durable
  # order; RunRecord carries no creation timestamp).
  run_finish_positions: dict[str, int] = field(default_factory=dict)
  # Input ids a successful run_finished acknowledged.
  confirmed_input_ids: set[str] = field(default_factory=set)
  # Input events inside the valid boundary (pre confirmation/claim filtering).
  input_candidates: list[dict] = field(default_factory=list)
  # Old pending input ids the task_imported boundary explicitly admits.
  imported_pending_ids: frozenset[str] = frozenset()
  # Absolute history position of the latest input boundary fact (creation,
  # import, close, or reopen); None before either fact exists.
  boundary_index: int | None = None
  # Whether inputs are admitted after the current boundary: True from
  # creation/import/reopen, False from a close (a closed node admits nothing;
  # arrivals during closure are history only).
  boundary_open: bool = True
  close_events: list[dict] = field(default_factory=list)
  close_requests: list[dict] = field(default_factory=list)
  # (child_session_id, child_event_id) pairs this session's log has received.
  delivered_reports: set[tuple[str, str]] = field(default_factory=set)
  # Every event id in the folded history (identity dedup for reports/inputs).
  event_ids: set[str] = field(default_factory=set)
  events_by_id: dict[str, dict] = field(default_factory=dict)
  covered: int = 0


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
      facts.task_state = "open"
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
      facts.task_state = str(event.get("outcome") or "completed")
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
      facts.task_state = "open"
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
  if etype not in INPUT_EVENT_TYPES:
    return False
  if etype == ET.USER:
    return is_real_user_message(event)
  return True


@dataclass(frozen=True)
class TaskTreeActivity:
  """One task-tree node's derived activity: the sidebar's truth for the node.

  ``has_running_tasks`` is true exactly while one of the node's Runs is live
  (recorded pid alive, no terminal fact); ``work_state`` is the node's
  fact-derived work verdict (idle | running | waiting) — waiting means a
  queued Run on an open task. Both come from one derivation —
  :func:`derive_task_tree_activity` — that ``TaskTreeManager.work_state_of``
  and the sidebar's task-tree probe share.
  """
  has_running_tasks: bool
  work_state: WorkState


def _run_outcomes(events: list[dict]) -> dict[str, str]:
  """The run-finished outcome map over one event list (last finish wins per run id)."""
  outcomes: dict[str, str] = {}
  for event in events:
    if event.get("type") != ET.RUN_FINISHED:
      continue
    run_id = event.get("run_id")
    if isinstance(run_id, str):
      outcomes[run_id] = str(event.get("outcome"))
  return outcomes


def derive_task_tree_activity(
    runs: list[RunRecord],
    events: list[dict],
    host_boot_time: Callable[[], datetime],
    task_open: bool,
    outcomes: dict[str, str] | None = None,
) -> TaskTreeActivity:
  """The single owner of a task-tree node's activity rules.

  Reads only durable facts (the run records and the session's fact history)
  plus process liveness for Runs that still lack a terminal fact — /proc is
  never consulted for a terminal or queued Run, and the host boot time is read
  lazily, only when some Run needs a liveness judgment. Running work wins over
  waiting work; every Run with a terminal fact contributes nothing, and so
  does a launched Run whose process is dead (its recovery is the
  completion/cancellation blockers' job, not a sidebar verdict).

  Waiting means a queued Run on an open task: ``task_open`` gates the queued
  verdict, because ``execute_run`` withholds every launch on a non-open task,
  so a closed task's queued Run never starts and must not hold the sidebar's
  clock. The
  running verdict is not gated: closure is refused while a Run is active, so a
  closed task cannot hold a live Run.
  """
  if not runs:
    return TaskTreeActivity(has_running_tasks=False, work_state="idle")
  if outcomes is None:
    outcomes = _run_outcomes(events)
  verdicts: list[str] = []
  has_running = False
  boot: datetime | None = None
  for run in runs:
    if outcomes.get(run.id) is not None:
      continue  # a terminal fact of any outcome is a settled Run, not activity
    if run.pid is None:
      if stop_requested_in_events(events, run.id):
        continue  # a stopped queued run is resolved-by-request, not waiting work
      if not task_open:
        continue  # a closed task's queued run never launches, so it is not waiting work
      verdicts.append("waiting")  # queued on an open task: retains its inputs for later dispatch
      continue
    # Liveness is judged only here, only for a launched Run without a terminal
    # fact — the sole case where a /proc read can change the verdict.
    if boot is None:
      boot = host_boot_time()
    if is_run_alive(run.pid, run.pid_start, run.started_at, boot):
      has_running = True
      verdicts.append("running")
    # A launched Run nobody observed exiting paints nothing: subagent failures
    # are routine and the manager resolves them by re-delegating, so a dead
    # process is not work the sidebar flags.
  work_state: WorkState = "idle"
  for state in ("running", "waiting"):
    if state in verdicts:
      work_state = state  # type: ignore[assignment]
      break
  return TaskTreeActivity(has_running_tasks=has_running, work_state=work_state)


class TaskTreeManager:
  """The task-tree record owner wired over one SessionManager."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
    self._cfg = cfg
    self._sessions = session_mgr
    self._store = session_mgr.store
    self.session_events = session_mgr.events
    session_mgr.task_tree_manager = self
    self.control_lock = asyncio.Lock()
    self.events = control_sink.ControlEventSink(self.session_events)
    self.runs = RunStore(cfg.sessions_dir, self.control_lock, self.events)
    # The run owner's terminal/stop/identity reads see the full fact history
    # (archived segments included), so a rotated acknowledgement never un-dones
    # itself and a repeat finish stays idempotent across rotation.
    self.runs.set_fact_history_loader(self.fact_history)
    # The run owner's launch/finish paths push a worker Run's busy interval
    # (the node's thinking_since) and its display backend into thinking_state.
    # The notification is the tree owner's because only the tree index knows
    # which nodes are workers.
    self.runs.set_liveness_notifier(self._note_run_liveness)
    self.dispatch = TaskInputDispatcher(self)
    self.completion = TaskCompletionManager(self)
    # The pending-input blockers of one session ([] when none): the structural
    # guard seam the input dispatcher answers.
    self.pending_input_blockers: Callable[[str], list[str]] | None = self.dispatch.pending_input_blockers
    # SessionManager-level writes that move a tree-projection input (the unread
    # flag in _set_unread_flag) drop this tree's rebuildable index through the
    # hook registered here — the same policy _save_meta applies to its own
    # metadata writes — so a tree page read after the flip never serves the
    # stale flag a missed broadcast would have left standing.
    session_mgr.tree_index_invalidator = self.invalidate_tree_index
    # The session lists read stored status; the archive of a task node is a
    # derived fact (archived_of, subtree inheritance included). The overlay
    # lets the sidebar's active list drop a delivered worker — and, with it,
    # every descendant of an archived ancestor — while its Archived list shows
    # them, with no status write.
    session_mgr.archive_overlay = self.derived_archived_ids
    # The sidebar's probe derives a task-tree node's activity through this
    # hook — the tree's own derivation, never a second copy of the rules.
    session_mgr.sidebar.task_tree_activity = self.activity_pair_of
    self._index: tuple[_TreeIndex, float] | None = None
    self._index_generation = 0
    self._index_build_task: asyncio.Task[_TreeIndex] | None = None
    self._index_build_generation = -1
    self._facts_memo: dict[str, tuple[list[dict], int, _TaskFacts]] = {}
    self._outcomes_memo: dict[str, tuple[list[dict], int, dict[str, str], int]] = {}
    # Activity cells: (records_generation, live events or None, covered live
    # length or -1, verdict). A None live list marks a runless node's cell —
    # its verdict is a constant that only a record write (a generation bump)
    # can move, so it skips the events load the runs-bearing key needs. The
    # covered length is in the key because the events cache takes an append in
    # place: the list identity survives a new fact, and unlike the suffix
    # folds this memo returns a stored verdict, so the only append a key
    # check can see is the length move. The archived extent needs no slot —
    # it rides the identity under the facts memo's contract (see _facts_of).
    self._activity_memo: dict[str, tuple[int, list[dict] | None, int, TaskTreeActivity]] = {}
    self._prompt_bodies_dir = cfg.charliebot_home / PROMPT_BODIES_DIR_NAME

  @property
  def prompt_bodies_dir(self) -> Path:
    """The immutable local-rule body store (prompt_bodies/<sha256>.md) in the selected home."""
    return self._prompt_bodies_dir

  @property
  def sessions(self) -> SessionManager:
    """The conversation/attachment service this tree is wired over."""
    return self._sessions

  @property
  def cfg(self) -> CharlieBotConfig:
    """The config this tree was built over (display-label resolution reads it)."""
    return self._cfg

  # ------------------------------------------------------------------
  # Metadata reads/writes (single owner of the v2 fields)
  # ------------------------------------------------------------------

  async def load_meta(self, session_id: str) -> SessionMetadata | None:
    return await self._store.get_session(session_id)

  def _require_task(self, meta: SessionMetadata | None, session_id: str) -> SessionMetadata:
    if meta is None:
      raise TaskNotFoundError(f"task {session_id} not found")
    return meta

  async def load_task_meta(self, session_id: str) -> SessionMetadata:
    """The session's metadata, required to be a task-tree node.

    Raises TaskNotFoundError for an absent session.
    """
    return self._require_task(await self.load_meta(session_id), session_id)

  async def _save_meta(self, meta: SessionMetadata) -> None:
    meta.updated_at = utc_now()
    await self._store.save_metadata(meta)
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
    cached_metas = self._store.fresh_cached_metas()
    task = create_logged_task(asyncio.to_thread(self._build_index_sync, cached_metas), name="task-tree-index-build")
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
    """A SessionManager-level write (legacy rename) changed tree inputs.

    Tree-node facts normally mutate through this owner (which invalidates in
    _save_meta); the legacy rename path writes through the SessionManager and
    must drop the projection cache here or tree_page serves a stale name."""
    self._invalidate_index()

  def _build_index_sync(self, cached_metas: dict[str, SessionMetadata]) -> _TreeIndex:
    sessions_dir = self._cfg.sessions_dir
    root_sig = (0, 0)
    try:
      st = os.stat(sessions_dir)
      root_sig = (st.st_mtime_ns, st.st_size)
    except OSError as e:
      raise RuntimeError(f"sessions dir unreadable at {sessions_dir}: {e}") from e
    metas: dict[str, SessionMetadata] = {}
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
      path = sessions_dir / name / METADATA_NAME
      try:
        raw = path.read_text(encoding="utf-8")
      except OSError:
        continue  # session dir without (yet readable) metadata: not a tree node
      try:
        metas[name] = validate_session_metadata(raw, str(path))
      except ValueError as e:
        raise RuntimeError(f"session metadata unparseable at {path}: {e}") from e
    children: dict[str | None, list[str]] = {}
    task_nodes: list[tuple[str, SessionMetadata]] = []
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
    pass_facts: dict[str, _TaskFacts] = {}
    structural: list[str] = []
    for sid, meta in task_nodes:
      facts = self._pass_facts(pass_facts, sid)
      structural.append(
          f"{sid}|{meta.task_parent_id or ''}|{meta.profile}|{meta.status.value}"
          f"|{facts.task_state}|{self._archived_of_pass(index, meta, memo, pass_facts)}")
    # The placeholder revision above is an input only to the shared-memo pass,
    # which never reads it; the real one lands before the index is published.
    index.revision = sha256_hex("\n".join(sorted(structural)))
    return index

  def _index_meta(self, index: _TreeIndex, session_id: str) -> SessionMetadata:
    meta = index.metas.get(session_id)
    if meta is None:
      raise TaskNotFoundError(f"task {session_id} not found")
    return meta

  def _children_of(self, index: _TreeIndex, session_id: str | None) -> list[str]:
    return list(index.children.get(session_id, []))

  def _descendants(self, index: _TreeIndex, session_id: str) -> list[str]:
    """All transitive descendants, cycle-guarded (a cycle here is corrupted data, not a tree)."""
    out: list[str] = []
    stack = list(self._children_of(index, session_id))
    seen = {session_id}
    while stack:
      sid = stack.pop()
      if sid in seen:
        raise TaskConflictError([f"task relation cycle through {sid}"])
      seen.add(sid)
      out.append(sid)
      stack.extend(self._children_of(index, sid))
      if len(out) > ANCESTOR_HOP_LIMIT:
        raise TaskConflictError([f"subtree of {session_id} exceeds {ANCESTOR_HOP_LIMIT} nodes"])
    return out

  def _ancestors(self, index: _TreeIndex, session_id: str) -> list[SessionMetadata]:
    """The task-parent chain above *session_id*, nearest first (cycle-guarded)."""
    chain: list[SessionMetadata] = []
    seen = {session_id}
    current = self._index_meta(index, session_id)
    while current.task_parent_id is not None:
      if current.task_parent_id in seen:
        raise TaskConflictError([f"task relation cycle through {current.task_parent_id}"])
      seen.add(current.task_parent_id)
      current = self._index_meta(index, current.task_parent_id)
      chain.append(current)
      if len(chain) > ANCESTOR_HOP_LIMIT:
        raise TaskConflictError([f"ancestor chain of {session_id} exceeds {ANCESTOR_HOP_LIMIT} hops"])
    return chain

  async def _note_run_liveness(self, session_id: str, run: RunRecord, launched: bool) -> None:
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
      clear_run_busy(session_id, run.id)
      return
    note_run_backend(session_id, run.backend)
    index = await self._get_index()
    meta = index.metas.get(session_id)
    if meta is not None and meta.profile == "worker":
      mark_run_busy(session_id, run.id, since=run.started_at)

  async def _require_open_ancestry(self, session_id: str) -> list[SessionMetadata]:
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

  def _archived_event_count(self, session_id: str, live: list[dict]) -> int:
    total = self.session_events.get_chat_event_count_sync(session_id)
    return max(0, total - len(live))

  def _load_archived_events(self, session_id: str, count: int) -> list[dict]:
    events, _has_more = self.session_events.load_chat_events_range(session_id, 0, count)
    return events

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

  def _run_outcomes_of(self, session_id: str, live: list[dict], archived_count: int) -> dict[str, str]:
    """The session's run-finished outcome map over the full history (suffix-memoized).

    Rides the facts memo's key (``_facts_of`` owns the extent-through-identity
    contract) — the live events cache's list identity — with the covered
    cursor in the cell: an append folds only the new suffix (a run_finished
    fact never un-happens, so the merge stays last-finish-wins), and a
    rotation's replaced list re-keys the whole fold.
    """
    cached = self._outcomes_memo.get(session_id)
    if cached is not None and cached[0] is live:
      outcomes, covered = cached[2], cached[3]
    else:
      outcomes = _run_outcomes(self._load_archived_events(session_id, archived_count)) if archived_count else {}
      covered = 0
    suffix = live[covered:]
    if suffix:
      for run_id, outcome in _run_outcomes(suffix).items():
        outcomes[run_id] = outcome
      covered = len(live)
    self._outcomes_memo[session_id] = (live, archived_count, outcomes, covered)
    return outcomes

  def _pass_facts(self, cache: dict[str, _TaskFacts], session_id: str) -> _TaskFacts:
    """The pass's facts view: one _facts_of consult chain per session id per cache.

    The build's structural read and the inheritance fold both need the same
    node's facts; the shared fold memo validates the events-cache identity on
    every call, so a pass that re-asks for an id it already holds re-pays the
    consult chains the first call settled.
    """
    facts = cache.get(session_id)
    if facts is None:
      facts = self._facts_of(session_id)
      cache[session_id] = facts
    return facts

  def facts_of(self, session_id: str) -> _TaskFacts:
    """Public fold entry for the input/completion owners (same memo)."""
    return self._facts_of(session_id)

  def task_state(self, session_id: str) -> str:
    """The task's derived lifecycle state."""
    return self._facts_of(session_id).task_state

  def work_state_of(self, session_id: str) -> WorkState:
    """idle | running | waiting, from CURRENT unresolved facts.

    One half of :meth:`activity_of` — the shared derivation's work verdict.
    Running work (a live launched Run) wins over waiting work (a queued Run on
    an open task); everything else — a Run with a terminal fact of any
    outcome, a queued Run on a closed task, or a launched Run whose process is
    dead — reads idle.
    """
    return self.activity_of(session_id).work_state

  def activity_of(self, session_id: str) -> TaskTreeActivity:
    """The node's derived sidebar activity: live-Run flag plus work verdict.

    The one derivation the sidebar's task-tree probe reuses, so a sidebar row
    and the tree projection can never disagree about the same node.

    Memoized per node on the inputs that can move the verdict: the run
    records' generation (every record mutation funnels through one
    :meth:`RunStore.write_record` bump) plus, for a node with runs, the
    chat-events identity and its covered length. The length is in the key
    because the events cache takes an append in place — identity survives a
    new fact, and a fact transition with no record write (a stop request, a
    close/reopen) moves only the length. The archived extent needs no key
    slot: it rides the identity under the facts memo's contract (see
    ``_facts_of``).
    A verdict that consulted /proc is never stored: a process death moves it
    with no file write to bump the key, so that node re-derives until its
    runs settle (the sidebar probe's recheck_liveness contract, unchanged).
    """
    generation = self.runs.records_generation
    cached = self._activity_memo.get(session_id)
    if cached is not None and cached[0] == generation:
      if cached[1] is None:
        return cached[3]
      live = self.session_events.load_chat_events_sync(session_id)
      if cached[1] is live and cached[2] == len(live):
        return cached[3]
    runs = self.runs.list_run_records_sync(session_id)
    if not runs:
      # No Run is no activity: the derivation's own guard answers without any
      # event load, so a listing's per-descendant derivation over a runless
      # node pays one runs-dir stat, not the node's whole event history.
      activity = derive_task_tree_activity([], [], self._host_boot_time, task_open=False)
      self._activity_memo[session_id] = (generation, None, -1, activity)
      return activity
    live = self.session_events.load_chat_events_sync(session_id)
    archived_count = self._archived_event_count(session_id, live)
    probed = False

    def host_boot_time() -> datetime:
      nonlocal probed
      probed = True
      return self._host_boot_time()

    events = self.runs.load_events_sync(session_id)
    task_open = self._facts_of(session_id).task_state == "open"
    activity = derive_task_tree_activity(
        runs, events, host_boot_time, task_open, outcomes=self._run_outcomes_of(session_id, live, archived_count))
    if probed:
      self._activity_memo.pop(session_id, None)
    else:
      self._activity_memo[session_id] = (generation, live, len(live), activity)
    return activity

  def activity_pair_of(self, session_id: str) -> tuple[bool, str]:
    """``activity_of`` as the plain pair the sidebar snapshot stores.

    The pair form keeps :mod:`src.runtime.sidebar_state` (which must not import
    the tree owner) free to hold and compare the value.
    """
    activity = self.activity_of(session_id)
    return (activity.has_running_tasks, activity.work_state)

  def _host_boot_time(self) -> datetime:
    from src.runtime.runs import read_host_boot_time
    return read_host_boot_time()

  def _archived_facts_based(self, meta: SessionMetadata, facts: _TaskFacts, cache: dict[str, _TaskFacts]) -> bool:
    """One node's OWN archive value from a pre-folded fact set (no inheritance).

    The subtree-inheritance fold around it lives in archived_of. A node is
    archived exactly when its task state is not open — the close fact is the
    archive, and a completed child counts at once (no wait for the parent receipt).
    The cache argument stays in the signature because the
    subtree pass hands one shared cache to every node's read.
    """
    return facts.task_state != "open"

  async def derived_archived_ids(self) -> set[str]:
    """Task nodes the effective archive lists while their stored status stays active.

    The read-time overlay the SessionManager listings apply: a task node whose
    state is not open (the close fact is the archive), and any node inherited
    under one, list as archived with no status write. Reads the cached index;
    a node already archived by status needs no overlay and is left out.
    Subtree inheritance rides the same call: one shared memo keeps the pass
    linear in node count.
    """
    index = await self._get_index()
    memo: dict[str, bool] = {}
    pass_facts: dict[str, _TaskFacts] = {}
    return {
        session_id for session_id, meta in index.metas.items()
        if meta.status != SessionStatus.ARCHIVED and self._archived_of_pass(index, meta, memo, pass_facts)
    }

  def archived_of(self, index: _TreeIndex, meta: SessionMetadata, memo: dict[str, bool] | None = None) -> bool:
    """Archive visibility with subtree inheritance (the effective archive's single owner).

    effective(n) is the node's own facts-based value or its parent's inherited
    one: a node is archived exactly when its task state is not open (the close
    fact is the archive — completed and cancelled included). Every descendant
    reads its parent's effective value, so an archived ancestor archives the subtree at read time with no descendant metadata
    write. A restored node is open again, so it unarchives. A shared *memo*
    makes a pass over many nodes
    linear in node count (each chain resolves through already-computed
    ancestors); a relation cycle surfaces through the hop guard as
    TaskConflictError, never silently.
    """
    return self._effective_archived(index, meta.id, {} if memo is None else memo, {})

  def _archived_of_pass(
      self, index: _TreeIndex, meta: SessionMetadata, memo: dict[str, bool], pass_facts: dict[str, _TaskFacts]) -> bool:
    """archived_of inside one caller-owned pass: the memo and the facts cache
    both span the caller's whole node walk, so chains already resolved and
    facts already consulted serve the rest of the pass."""
    return self._effective_archived(index, meta.id, memo, pass_facts)

  def _effective_archived(
      self, index: _TreeIndex, session_id: str, memo: dict[str, bool], pass_facts: dict[str, _TaskFacts]) -> bool:
    """One node's effective archive value with inheritance, memoized per pass.

    Walks up the task-parent chain to the first memoized node or a root, then
    folds the effective values back down; every node the walk touches lands in
    the memo, so a whole-listing pass never recomputes one. The fold reads
    facts through *pass_facts*, and an inherited True settles the subtree
    without any node's own facts read. Inheritance never overrides a node's
    own open state into visibility: an archived ancestor archives the subtree
    (and the invariant forbids the reverse shape — an open node under an
    archived ancestor).
    """
    chain: list[tuple[str, SessionMetadata]] = []
    seen: set[str] = set()
    current = session_id
    inherited = False
    while True:
      known = memo.get(current)
      if known is not None:
        inherited = known
        break
      if current in seen or len(chain) > ANCESTOR_HOP_LIMIT:
        raise TaskConflictError([f"task relation cycle through {current}"])
      seen.add(current)
      meta = self._index_meta(index, current)
      chain.append((current, meta))
      if meta.task_parent_id is None:
        inherited = False  # a root inherits nothing
        break
      current = meta.task_parent_id
    for sid, node in reversed(chain):
      if not inherited:
        inherited = self._archived_facts_based(node, self._pass_facts(pass_facts, sid), pass_facts)
      memo[sid] = inherited
    return memo[session_id]

  def session_row(self, index: _TreeIndex, session_id: str) -> SessionRow:
    meta = self._index_meta(index, session_id)
    child_ids = self._children_of(index, session_id)
    descendants = self._descendants(index, session_id)
    descendant_work = [self.work_state_of(d) for d in descendants]
    open_count = sum(1 for d in descendants if self.task_state(d) == "open")
    running_count = sum(1 for state in descendant_work if state == "running")
    return SessionRow(
        id=meta.id,
        name=meta.name,
        profile=meta.profile,
        task_parent_id=meta.task_parent_id,
        task_state=self.task_state(session_id),  # type: ignore[arg-type]
        work_state=self.work_state_of(session_id),
        archived=self.archived_of(index, meta),
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
    (``SessionManager.persist_native_anchor_provenance``) — every authorized
    writer of ``native_backend`` is an anchor write, so a concurrent stale
    whole-object save cannot roll the identity back. A failed preparation never
    reaches this write, so the usable old anchor survives it.
    """
    async with self.control_lock:
      await self._sessions.persist_native_anchor_provenance(
          session_id, prompt_hash=prompt_hash, native_backend=backend, model=model)
      self._invalidate_index()  # any metadata write may move the projection inputs
      if reset_anchor:
        fresh = await self._store.read_metadata_fresh(session_id)
        if fresh is not None and fresh.cc_session_id is not None:
          # The fresh native context voids the old conversation anchor. The clear
          # goes through the authorized channel: a whole-object save's anchor
          # reconciliation would correct it back to the disk value and the reset
          # would silently do nothing.
          await self._sessions.clear_cc_session_anchor(session_id)

  def prompt_rule_summaries(self, meta: SessionMetadata, index: _TreeIndex) -> dict:
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
        "affected_descendants": len(self._descendants(index, meta.id)),
    }

  async def session_detail(self, session_id: str) -> dict:
    """The session detail projection: existing metadata plus the derived task fields."""
    await self.load_task_meta(session_id)
    index = await self._get_index()
    # Re-read through the index so the detail and the tree agree on one projection.
    indexed = index.metas.get(session_id)
    if indexed is None:
      raise TaskNotFoundError(f"task {session_id} not found")
    ancestors = self._ancestors(index, session_id)
    payload = indexed.model_dump(mode="json")
    payload.update(
        {
            "task_state": self.task_state(session_id),
            "work_state": self.work_state_of(session_id),
            "archived": self.archived_of(index, indexed),
            "ancestors": [AncestorRef(id=a.id, name=a.name).model_dump() for a in ancestors],
            "prompt_rules": self.prompt_rule_summaries(indexed, index),
        })
    return payload

  # ------------------------------------------------------------------
  # Create
  # ------------------------------------------------------------------

  async def adopt_metadata_slot(
      self,
      session_id: str,
      old: SessionMetadata,
      owner: str,
      *,
      fields: tuple[str, ...] | None = None,
  ) -> SessionMetadata:
    """Copy selected fields from *owner* on *old* onto this node."""
    async with self.control_lock:
      meta = await self.load_meta(session_id)
      if meta is None:
        raise TaskNotFoundError(f"session {session_id} not found")
      metadata_slots.copy_fields(old, meta, owner, names=fields)
      meta.updated_at = utc_now()
      await self._save_meta(meta)
      return meta

  async def create_task(
      self,
      *,
      request_id: str,
      task_parent_id: str | None,
      profile: str,
      task: TaskSpec | None,
      name: str | None,
      backend: str | None,
      group: str | None = None,
      session_id: str | None = None,
      slot_values: dict[str, Any] | None = None,
      caller: object,
  ) -> SessionMetadata:
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
      raise TaskInvalidError(TASK_CREATE_REQUEST_ID_REQUIRED)
    if profile not in ("manager", "worker"):
      raise TaskInvalidError("profile must be 'manager' or 'worker'")
    if session_id is not None and _create_actor_for(caller) == ACTOR_AGENT:
      raise TaskForbiddenError(AGENT_CREATE_SCOPE_REFUSAL)
    task_id = session_id or stable_task_id(task_parent_id, request_id)
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
        if isinstance(caller, CallerIdentity) and not caller.is_operator:
          parent_meta = await self.load_meta(task_parent_id) if task_parent_id is not None else None
          await self._authorize_agent_creation(caller, existing.profile, existing.task, task_parent_id, parent_meta)
        return existing
      parent_meta: SessionMetadata | None = None
      # Caller scope first: an agent's 403 must not depend on the target's shape.
      if isinstance(caller, CallerIdentity) and not caller.is_operator:
        if task_parent_id is not None:
          parent_meta = await self.load_meta(task_parent_id)
        await self._authorize_agent_creation(caller, profile, task, task_parent_id, parent_meta)
      if task_parent_id is not None:
        parent_meta = await self.load_meta(task_parent_id)
        if parent_meta is None:
          raise TaskNotFoundError(f"task {task_parent_id} not found")
        if parent_meta.profile == "worker":
          raise TaskInvalidError(f"parent task {task_parent_id} is not a manager")
        index = await self._get_index()
        parent_state = self.task_state(task_parent_id)
        if parent_state != "open":
          raise TaskConflictError([f"parent task {task_parent_id} is {parent_state}"])
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
    self._store.invalidate_cache(task_id)
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

  async def _default_node_name(self, task: TaskSpec | None, profile: str) -> str:
    """A node created without a name takes the goal's first line when there is
    one; otherwise the session counter name ("Session N"), so the
    sidebar's one-click create names a task node the way it names a session.
    """
    if task is not None and task.goal.strip():
      return default_task_name(task, profile)
    return await self._sessions._next_session_name()

  async def _authorize_agent_creation(
      self,
      caller: object,
      profile: str,
      task: TaskSpec | None,
      task_parent_id: str | None,
      parent_meta: SessionMetadata | None,
  ) -> None:
    """An agent caller may organize only its own open manager task.

    A logical manager child directly under the caller's own manager task is
    coordination work and needs no user authorization; a worker child is
    implementation delegation and still rides the nearest-real-user-ancestor
    gate (takeoff_gate) — except the read-only verify delegation, which the
    one shared judgment (takeoff_gate.is_verify_exempt) excuses here exactly
    as it does on the route and at the launch. On a replayed create *task* is
    the ORIGINAL node's spec, so re-labeling a replay cannot borrow the
    exemption for a node created as implementation. Any other shape — an
    unrelated root, a foreign parent, a worker parent — is
    outside an agent's scope.
    """
    assert isinstance(caller, CallerIdentity)
    claims = caller.claims
    assert claims is not None
    if (task_parent_id != claims.session_id or parent_meta is None or parent_meta.profile != "manager"):
      raise TaskForbiddenError(AGENT_CREATE_SCOPE_REFUSAL)
    if profile == "worker" and not is_verify_exempt(task):
      # Implementation authorization stays with the caller's own manager task:
      # the nearest-real-user-ancestor gate (takeoff_gate) decides. The
      # read-only verify delegation needs no window, on the same task-type
      # judgment the route and the launch apply.
      await self.check_task_authorization(claims.session_id)

  async def _require_open_ancestry_from_index(self, index: _TreeIndex, session_id: str) -> list[SessionMetadata]:
    """Every ancestor of *session_id* must be an open task (API: 409 otherwise)."""
    chain = self._ancestors(index, session_id)
    closed = [a.id for a in chain if self._facts_of(a.id).task_state != "open"]
    if closed:
      raise TaskConflictError([closed_ancestors_blocker(closed)])
    return chain

  async def _publish_new_task(
      self,
      *,
      task_id: str,
      request_id: str,
      task_parent_id: str | None,
      profile: str,
      task: TaskSpec | None,
      name: str | None,
      backend: str | None,
      group: str | None,
      slot_values: dict[str, Any],
      parent_meta: SessionMetadata | None,
      actor: str,
  ) -> SessionMetadata:
    sessions_dir = self._cfg.sessions_dir
    sessions_dir.mkdir(parents=True, exist_ok=True)
    final_dir = sessions_dir / task_id
    temp_dir = sessions_dir / f".task-{task_id}-{os.getpid()}-{uuid.uuid4().hex}.tmp"
    meta = SessionMetadata(
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
      (temp_dir / DATA_DIR_NAME).mkdir(parents=True)
      # The creation fact is written into the temp node itself, so metadata and
      # the event log publish together with the one rename; post-publication
      # facts go through the sink.
      created_event = build_task_created_event(
          actor=actor,
          task_id=task_id,
          request_id=request_id,
          task_parent_id=task_parent_id,
          task_spec_hash=canonical_task_spec_hash(task),
      )
      await append_ndjson(chat_events_path(temp_dir), created_event)
      meta.created_by_event = EventRef(session_id=task_id, event_id=str(created_event["id"]))
      await asyncio.to_thread(
          atomic_write_text, temp_dir / METADATA_NAME,
          meta.model_dump_json(indent=2, exclude=TRANSIENT_METADATA_FIELDS))
      try:
        os.replace(temp_dir, final_dir)
      except OSError:
        # Lost a create race for the same stable id: the original product exists.
        existing = self._read_metadata_file(final_dir / METADATA_NAME)
        if existing is None:
          raise
        return existing
      return meta
    finally:
      if temp_dir.exists():
        shutil.rmtree(temp_dir, ignore_errors=True)

  @staticmethod
  def _read_metadata_file(path) -> SessionMetadata | None:
    if not path.exists():
      return None
    return validate_session_metadata(path.read_text(encoding="utf-8"), str(path))

  async def check_task_authorization(self, session_id: str, now: datetime | None = None) -> str:
    """The nearest-real-user-ancestor gate for a v2 task caller (takeoff_gate).

    Every ancestor must be open and the calling node a manager; the first node
    holding a real user instruction is where the time-window rules
    apply, and a failure there blocks without borrowing from higher ancestors.
    """
    from src.runtime.takeoff_gate import check_takeoff_gate_for_task
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

    return check_takeoff_gate_for_task(
        session_id,
        load_events=self.events.load_events,
        task_meta_of=meta_of,
        task_state_of=state_of,
        now=now,
    )

  async def update_slot_fields(self, session_id: str, owner: str, **values: Any) -> SessionMetadata:
    """Write registered metadata-slot fields without refreshing the sidebar sort key.

    The node is re-read under the control lock, so a concurrent task edit
    between the caller's earlier read and this write is preserved. Saving
    directly leaves ``updated_at`` in place while still dirtying the sidebar,
    bumping the listing revision and invalidating the tree index.
    """
    if not values:
      raise TaskInvalidError("update_slot_fields requires at least one field")
    async with self.control_lock:
      meta = await self.load_meta(session_id)
      if meta is None:
        raise TaskNotFoundError(f"session {session_id} not found")
      metadata_slots.set_fields(meta, owner, **values)
      await self._store.save_metadata(meta)
      self._invalidate_index()
      return meta

  async def create_retry(self, session_id: str, request_id: str, original_run_id: str) -> dict:
    """Create one retry run on the open task; a replayed request returns the same run.

    The retry pins the task's current spec text (later edits keep the old
    versions pinned to their own runs) and inherits the retried run's
    execution context; the previous run's evidence is preserved untouched.
    """
    async with self.control_lock:
      await self._get_index()
      meta = await self.load_task_meta(session_id)
      state = self.task_state(session_id)
      if state != "open":
        raise TaskConflictError([f"task {session_id} is {state}; only open tasks accept retries"])
      original = await self.runs.get_run(session_id, original_run_id)
      if original is None:
        raise TaskNotFoundError(f"run {original_run_id} not found in session {session_id}")
      retry_fields: dict = {}
      if original.kind == "manager_turn":
        # A manager-round retry reruns that round's own batch: the retry Run
        # binds the original's input_event_ids, so the launch path skips the
        # pending claim (the original batch counts as handled) and the finish
        # payload is exactly that batch. Worker retries stay claimless — they
        # consume whatever is pending when they launch.
        retry_fields["input_event_ids"] = list(original.input_event_ids)
      run = await self.runs.create_retry_run_locked(
          session_id,
          request_id,
          original_run_id,
          task_spec_text=canonical_task_spec_text(meta.task),
          kind=original.kind,
          backend=original.backend,
          model=original.model,
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

  async def patch_task(self, session_id: str, req: PatchSessionTaskRequest, *, caller: object) -> SessionMetadata:
    """Apply one v2 metadata mutation with its structural guards (operator-only)."""
    require_operator(caller, "task metadata mutations require operator credentials")
    async with self.control_lock:
      await self._get_index()
      meta = await self.load_task_meta(session_id)
      fs = req.model_fields_set
      structural = bool(fs & {"task", "profile", "task_parent_id"})
      blockers = self._structural_blockers(session_id) if structural else []
      if ("profile" in fs and req.profile is not None and req.profile != meta.profile and req.profile == "worker" and
          self._children_count(session_id) > 0):
        blockers.append("demotion to worker requires a task with no child tasks")
      if "task_parent_id" in fs and req.task_parent_id != meta.task_parent_id:
        blockers.extend(await self._reparent_blockers(session_id, req.task_parent_id))
      if blockers:
        raise TaskConflictError(sorted(set(blockers)))
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

  def _children_count(self, session_id: str) -> int:
    if self._index is None:
      raise RuntimeError("tree index must be built before structural guards")
    return len(self._children_of(self._index[0], session_id))

  def _structural_blockers(self, session_id: str) -> list[str]:
    """Running and pending-execution blockers of one node (run facts; input seam extends)."""
    blockers: list[str] = []
    if self.pending_input_blockers is not None:
      blockers.extend(self.pending_input_blockers(session_id))
    runs = self.runs.list_run_records_sync(session_id)
    if not runs:
      return blockers
    events = self.runs.load_events_sync(session_id)
    host_boot = self._host_boot_time()
    for run in runs:
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

  async def _apply_prompt_change(self, session_id: str, meta: SessionMetadata, scope: str, body: str | None) -> None:
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

  async def _ensure_prompt_changed_fact(self, session_id: str, meta: SessionMetadata, scope: str) -> None:
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
        build_control_event(
            ET.PROMPT_CHANGED,
            actor=ACTOR_USER,
            source_session_id=session_id,
            scope=scope,
            previous_ref=last.get("new_ref") if last is not None else None,
            new_ref=current,
        ))

  def _store_prompt_body(self, body: str) -> str:
    """Write the rule body once under its SHA-256 fingerprint; returns the fingerprint ref."""
    ref = sha256_hex(body)
    path = self._prompt_bodies_dir / f"{ref}.md"
    if path.exists():
      existing = path.read_text(encoding="utf-8")
      if existing != body:
        raise RuntimeError(f"prompt body store corrupted at {path}: fingerprint content mismatch")
      return ref
    path.parent.mkdir(parents=True, exist_ok=True)
    atomic_write_text(path, body)
    return ref

  # ------------------------------------------------------------------
  # Tree queries
  # ------------------------------------------------------------------

  def _has_running_work_descendant(self, index: _TreeIndex, session_id: str) -> bool:
    """True when any descendant's current work is running."""
    return any(self.work_state_of(d) == "running" for d in self._descendants(index, session_id))

  async def tree_page(
      self,
      *,
      parent_id: str | None,
      include_archived: bool,
      limit: int,
      cursor: str | None,
  ) -> dict:
    """One revision-bound keyset page of the task tree; stale pagination is a 409."""
    index = await self._get_index()
    if parent_id:
      self._index_meta(index, parent_id)
    children = self._children_of(index, parent_id or None)
    rows_all = [self.session_row(index, sid) for sid in children]
    if not include_archived:
      # An archived row stays navigable while running work lives at or below
      # it: dropping it would sever the path to that work in a partial client
      # tree. The row's OWN work state counts — under inheritance a running
      # leaf below a retained archived parent is itself archived, and only its
      # own state keeps it (and with it the parent's child page) visible. The
      # row still reports archived=true.
      rows_all = [
          r for r in rows_all
          if not r.archived or r.work_state == "running" or self._has_running_work_descendant(index, r.id)
      ]
    rows_all.sort(key=lambda r: (index.metas[r.id].created_at, r.id))
    after = _decode_tree_cursor(cursor) if cursor else None
    if after is not None:
      after_revision, after_key = after
      if after_revision != index.revision:
        raise TaskConflictError(["task tree changed during pagination; refresh and re-paginate"])
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

  async def deletion_blockers(self, session_id: str) -> list[str]:
    """Permanent delete requires an empty, unreferenced task (the check half)."""
    index = await self._get_index(force=True)  # a just-saved reference must be seen
    self._index_meta(index, session_id)
    meta = index.metas[session_id]
    return self._deletion_blockers_locked(index, session_id)

  def _deletion_reference_blockers(self, index: _TreeIndex, session_id: str) -> list[str]:
    """Saved child, run, and trigger references that prevent deletion."""
    blockers: list[str] = []
    children = self._children_of(index, session_id)
    if children:
      blockers.append(f"has child task(s): {', '.join(children)}")
    runs = self.runs.list_run_records_sync(session_id)
    if runs:
      blockers.append(f"has run record(s): {', '.join(r.id for r in runs)}")
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
    require_operator(caller, "permanent delete requires operator credentials")
    async with self.control_lock:
      index = await self._get_index(force=True)
      self._index_meta(index, session_id)
      blockers = self._deletion_blockers_locked(index, session_id)
      if blockers:
        raise TaskConflictError(sorted(set(blockers)))
      result = await self._sessions.delete_session_permanently(session_id)
    if result:
      self._invalidate_index()
      self._facts_memo.pop(session_id, None)
      self._outcomes_memo.pop(session_id, None)
      self._activity_memo.pop(session_id, None)
    return result

  # ------------------------------------------------------------------
  # Archive: the user's single end state, cascading down the subtree
  # ------------------------------------------------------------------

  async def archive_subtree(self, session_id: str, *, caller: object) -> list[str]:
    """Archive *session_id*'s whole subtree: one archived close fact per open node.

    Operator scope only. The control lock holds for the whole operation: the
    subtree is collected and every unfinished run is refused before anything
    is written, so a refused call leaves zero facts and a passed call leaves
    no window between the facts. Each open node's fact carries outcome
    "archived", actor "user", and an empty report_to — an archive reports to
    nobody; each descendant's fact also names the node the user archived in
    ``archived_with``. Completed and cancelled nodes are left unchanged. Each
    archived node's sequence binding receives its archive callback. Returns
    the ids this call archived, in parent-before-child order; an already-archived target returns [].
    """
    from src.runtime.hooks.sequence_controllers import binding_for

    require_operator(caller, "archiving a task requires operator credentials")
    tree = self
    async with tree.control_lock:
      index = await tree._get_index(force=True)
      tree._index_meta(index, session_id)
      if tree.task_state(session_id) != "open":
        return []  # the target is already archived; the invariant empties its subtree too
      subtree = [session_id, *tree._descendants(index, session_id)]
      # Refuse before writing: every node in the subtree must be free of an
      # unfinished run (terminal facts settle a run; queued and unresolved
      # ones block like any structural change).
      blocked: list[str] = []
      host_boot = tree._host_boot_time()
      for sid in subtree:
        for run in tree.runs.list_run_records_sync(sid):
          blocker = tree.runs.run_blocker(run, tree.runs.load_events_sync(sid), host_boot)
          if blocker is not None:
            blocked.append(f"{sid}: {blocker}")
      if blocked:
        raise TaskConflictError(sorted(blocked))
      request_id = f"archive-{uuid.uuid4()}"
      archived: list[str] = []
      # Parent before child: the fold reads each node's own facts, so the
      # order is convention, but it keeps every intermediate state valid.
      for sid in subtree:
        if tree.task_state(sid) != "open":
          continue
        event = build_control_event(
            ET.TASK_CLOSED,
            actor=ACTOR_USER,
            source_session_id=sid,
            event_id=stable_close_event_id(sid, request_id),
            request_id=request_id,
            outcome="archived",
            summary="archived by the user" if sid == session_id else "",
            result_refs=[],
            run_ids=[],
            report_to=None,
            **({
                "archived_with": session_id
            } if sid != session_id else {}),
        )
        await tree.events.append(sid, event)
        archived.append(sid)
      tree._invalidate_index()
      # The archived nodes' sequence bindings run their archive duties.
      for sid in archived:
        binding = binding_for(sid)
        if binding is not None:
          await binding.on_archive()
      return archived


# ---------------------------------------------------------------------------
# Module helpers
# ---------------------------------------------------------------------------


def _create_actor_for(caller: object) -> str:
  """The creation fact's actor for one create_task caller.

  A verified operator is a user; the configured scheduler's server-owned fire
  passes the "system" sentinel (provenance the server owns — never a payload
  bit a run-token caller can forge); anything else is an agent.
  """
  if isinstance(caller, CallerIdentity) and caller.is_operator:
    return ACTOR_USER
  if caller == "system":
    return ACTOR_SYSTEM
  return ACTOR_AGENT


def canonical_task_spec_text(task: TaskSpec | None) -> str | None:
  """The canonical serialized task-spec body a run pins (and hashes) at launch."""
  if task is None:
    return None
  return orjson.dumps(task.model_dump()).decode()


def canonical_task_spec_hash(task: TaskSpec | None) -> str | None:
  """The SHA-256 fingerprint of the canonical task-spec body (None without a spec)."""
  text = canonical_task_spec_text(task)
  return sha256_hex(text) if text is not None else None


@dataclass
class _TreeIndex:
  """The rebuildable parent/child index over one sessions directory."""
  metas: dict[str, SessionMetadata]
  children: dict[str | None, list[str]]
  revision: str
  root_sig: tuple[int, int]


# A Markdown ATX heading: up to three leading spaces, then 1-6 '#' closed by
# whitespace or end of line (so "####### tag" is content, not a heading).
_MD_HEADING_RE = re.compile(r"^\s{0,3}#{1,6}(\s|$)")


def default_task_name(task: TaskSpec | None, profile: str) -> str:
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


def jsonable_row(row: SessionRow) -> dict:
  return row.model_dump()


def _encode_tree_cursor(revision: str, key: tuple[datetime, str]) -> str:
  """Revision-bound keyset cursor: base64url JSON of (revision, created_at, id)."""
  payload = {"r": revision, "a": key[0].isoformat(), "i": key[1]}
  return b64url_encode(orjson.dumps(payload))


def _decode_tree_cursor(cursor: str) -> tuple[str, tuple[datetime, str]]:
  """Decode one tree cursor; a malformed value fails loud (API: 400)."""
  try:
    payload = orjson.loads(b64url_decode(cursor))
    key = (ensure_utc(datetime.fromisoformat(payload["a"])), payload["i"])
    return payload["r"], key
  except (ValueError, KeyError, TypeError) as e:
    raise TaskInvalidError(f"malformed tree page cursor: {cursor!r}") from e
