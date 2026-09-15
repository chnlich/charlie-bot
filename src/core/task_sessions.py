"""Task-tree owner: v2 task metadata mutations, tree validation, derived queries.

This module is the single owner of schema_version=2 task metadata (everything
``SessionMetadata`` carries beyond the v1 session fields), of the task-tree
relation (``task_parent_id`` edges over flat ``sessions/<id>`` directories),
and of the derived task projection (``task_state`` / ``work_state`` / archive
visibility / subtree counts). The legacy conversation, attachment, rating and
page-aggregation services stay in :mod:`src.core.sessions`.

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
- Input delivery (``src.core.session_dispatch``) and completion guards
  (``src.core.task_completion``) own their policies and join the same control
  lock; this module hosts their wiring, the fact-history reader they share,
  and the pending-input blocker hook they answer.
"""

import asyncio
import base64
import os
import shutil
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from typing import TYPE_CHECKING

import orjson

from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.control_events import (
  ACTOR_AGENT,
  ACTOR_SYSTEM,
  ACTOR_USER,
  ControlEventSink,
  build_control_event,
  sha256_hex,
  stable_task_id,
)
from src.core.json_utils import atomic_write_text
from src.core.models import (
  AncestorRef,
  EventRef,
  PatchSessionTaskRequest,
  SessionMetadata,
  SessionRow,
  SessionStatus,
  TaskSpec,
  WorkState,
  utc_now,
)
from src.core.ndjson import append_ndjson
from src.core.run_token import CallerIdentity
from src.core.runs import RunStore
from src.core.session_aliases import SessionAliasStore
from src.core.session_dispatch import TaskInputDispatcher
from src.core.sessions import _TRANSIENT_METADATA_FIELDS, SessionManager
from src.core.task_completion import TaskCompletionManager

if TYPE_CHECKING:
  pass

PROMPT_BODIES_DIR_NAME = "prompt_bodies"

# The rebuildable index self-heals on this cadence even when nothing the owner
# wrote moved the sessions root: one metadata walk per TTL per process is the
# bound an out-of-band edit (none exists in the single-service model) could
# stay invisible for.
_TREE_INDEX_TTL_SECONDS = 2.0

# Bound on the ancestor walk: open-ancestor checks and ancestor paths must
# never spin on a corrupted relation.
_ANCESTOR_HOP_LIMIT = 1000


class TaskInvalidError(ValueError):
  """Empty target or illegal relation (API: 400)."""


class TaskNotFoundError(LookupError):
  """The referenced task/Run does not exist (API: 404)."""


class TaskForbiddenError(PermissionError):
  """The caller's identity or role does not allow the operation (API: 403)."""


class TaskConflictError(Exception):
  """Concurrent change or lifecycle conflict with concrete blockers (API: 409)."""

  def __init__(self, blockers: list[str]) -> None:
    self.blockers = blockers
    super().__init__("; ".join(blockers))


@dataclass
class _TaskFacts:
  """The derived facts one session's full event history folds to.

  The fold is the single derived-fact owner for the tree projection, the
  input dispatcher, and the completion owner: task lifecycle, run outcomes,
  input candidacy with its boundary, close/reopen/close-request facts, and
  delivered child reports all come from this one pass over the durable
  events (archived segments included).
  """
  task_state: str = "open"
  run_outcomes: dict[str, str] = field(default_factory=dict)
  # Input ids a successful run_finished acknowledged.
  confirmed_input_ids: set[str] = field(default_factory=set)
  # Input events inside the valid boundary (pre confirmation/claim filtering).
  input_candidates: list[dict] = field(default_factory=list)
  # Old pending input ids the task_imported boundary explicitly admits.
  imported_pending_ids: frozenset[str] = frozenset()
  # Absolute history position of the creation (or import) boundary; None
  # before either fact exists.
  boundary_index: int | None = None
  close_events: list[dict] = field(default_factory=list)
  reopen_events: list[dict] = field(default_factory=list)
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
    if etype == ET.TASK_CREATED:
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
          raise ValueError(
              f"task_imported event {event_id!r} lists a pending input without a stable input_id")
        listed.append(input_id)
      facts.boundary_index = absolute
      facts.imported_pending_ids = frozenset(listed)
      # The boundary moved: imported tasks admit post-import input plus ONLY
      # the old pending inputs the boundary explicitly lists — pre-boundary
      # candidacy narrows to that declared set, never the whole history.
      facts.input_candidates = [
          e for e in facts.events_by_id.values()
          if e.get("type") in _INPUT_EVENT_TYPES and e.get("id") in facts.imported_pending_ids]
    elif etype == ET.TASK_CLOSED:
      facts.task_state = str(event.get("outcome") or "completed")
      facts.close_events.append(event)
    elif etype == ET.TASK_REOPENED:
      facts.task_state = "open"
      facts.reopen_events.append(event)
    elif etype == ET.TASK_CLOSE_REQUESTED:
      facts.close_requests.append(event)
    elif etype == ET.RUN_FINISHED:
      run_id = event.get("run_id")
      if isinstance(run_id, str):
        facts.run_outcomes[run_id] = str(event.get("outcome"))
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
    if etype in _INPUT_EVENT_TYPES:
      if facts.boundary_index is None or absolute > facts.boundary_index or (
          event_id is not None and event_id in facts.imported_pending_ids):
        facts.input_candidates.append(event)
  return facts


_INPUT_EVENT_TYPES = frozenset({
    ET.USER, ET.AGENT_MESSAGE, ET.SCHEDULED_TRIGGER, ET.CHILD_REPORT})


class TaskTreeManager:
  """The task-tree record owner wired over one SessionManager."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
    self._cfg = cfg
    self._sessions = session_mgr
    self.control_lock = asyncio.Lock()
    self.events = ControlEventSink(session_mgr)
    self.aliases = SessionAliasStore(cfg.sessions_dir)
    self.runs = RunStore(cfg, self.control_lock, self.events, self.aliases)
    # The run owner's terminal/stop/identity reads see the full fact history
    # (archived segments included), so a rotated acknowledgement never un-dones
    # itself and a repeat finish stays idempotent across rotation.
    self.runs.set_fact_history_loader(self.fact_history)
    self.dispatch = TaskInputDispatcher(self)
    self.completion = TaskCompletionManager(self)
    # The pending-input blockers of one session ([] when none): the structural
    # guard seam the input dispatcher answers.
    self.pending_input_blockers: Callable[[str], list[str]] | None = self.dispatch.pending_input_blockers
    self._index: tuple[_TreeIndex, float] | None = None
    self._index_generation = 0
    self._facts_memo: dict[str, tuple[list[dict], int, _TaskFacts]] = {}
    self._prompt_bodies_dir = cfg.charliebot_home / PROMPT_BODIES_DIR_NAME

  @property
  def sessions(self) -> SessionManager:
    """The conversation/attachment service this tree is wired over."""
    return self._sessions

  # ------------------------------------------------------------------
  # Metadata reads/writes (single owner of the v2 fields)
  # ------------------------------------------------------------------

  async def load_meta(self, session_id: str) -> SessionMetadata | None:
    return await self._sessions.get_session(session_id)

  def _require_task(self, meta: SessionMetadata | None, session_id: str) -> SessionMetadata:
    if meta is None:
      raise TaskNotFoundError(f"task {session_id} not found")
    if meta.profile is None:
      raise TaskInvalidError(f"session {session_id} is not a task-tree node (no profile)")
    return meta

  async def _save_meta(self, meta: SessionMetadata) -> None:
    meta.updated_at = utc_now()
    await self._sessions.save_metadata(meta)
    self._invalidate_index()  # any metadata write may move the projection inputs

  # ------------------------------------------------------------------
  # Tree index (rebuildable)
  # ------------------------------------------------------------------

  async def _get_index(self, *, force: bool = False) -> "_TreeIndex":
    now = time.monotonic()
    if not force and self._index is not None and now - self._index[1] < _TREE_INDEX_TTL_SECONDS:
      return self._index[0]
    generation = self._index_generation
    index = await asyncio.to_thread(self._build_index_sync)
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

  def _build_index_sync(self) -> "_TreeIndex":
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
      path = sessions_dir / name / "metadata.json"
      try:
        raw = path.read_text(encoding="utf-8")
      except OSError:
        continue  # session dir without (yet readable) metadata: not a tree node
      try:
        metas[name] = SessionMetadata.model_validate_json(raw)
      except ValueError as e:
        raise RuntimeError(f"session metadata unparseable at {path}: {e}") from e
    children: dict[str | None, list[str]] = {}
    structural: list[str] = []
    for sid, meta in metas.items():
      if meta.profile is None:
        continue  # legacy v1 session: not a task-tree node
      children.setdefault(meta.task_parent_id, []).append(sid)
      # The revision covers every input of archive membership (the tree page's
      # row filter), so a facts-driven membership change during pagination is
      # a visible 409 instead of a silently omitted or repeated row.
      facts = self._facts_of(sid)
      structural.append(
          f"{sid}|{meta.task_parent_id or ''}|{meta.profile}|{meta.presentation}|{meta.status.value}"
          f"|{facts.task_state}|{self._archived_facts_based(meta, facts)}")
    for kids in children.values():
      kids.sort(key=lambda sid: (metas[sid].created_at, sid))
    revision_input = "\n".join(sorted(structural))
    revision = sha256_hex(revision_input)
    return _TreeIndex(metas=metas, children=children, revision=revision, root_sig=root_sig)

  def _index_meta(self, index: "_TreeIndex", session_id: str) -> SessionMetadata:
    meta = index.metas.get(session_id)
    if meta is None:
      raise TaskNotFoundError(f"task {session_id} not found")
    return meta

  def _children_of(self, index: "_TreeIndex", session_id: str | None) -> list[str]:
    return list(index.children.get(session_id, []))

  def _descendants(self, index: "_TreeIndex", session_id: str) -> list[str]:
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
      if len(out) > _ANCESTOR_HOP_LIMIT:
        raise TaskConflictError([f"subtree of {session_id} exceeds {_ANCESTOR_HOP_LIMIT} nodes"])
    return out

  def _ancestors(self, index: "_TreeIndex", session_id: str) -> list[SessionMetadata]:
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
      if len(chain) > _ANCESTOR_HOP_LIMIT:
        raise TaskConflictError([f"ancestor chain of {session_id} exceeds {_ANCESTOR_HOP_LIMIT} hops"])
    return chain

  async def _require_open_ancestry(self, session_id: str) -> list[SessionMetadata]:
    """Every ancestor of *session_id* must be an open task (API: 409 otherwise)."""
    index = await self._get_index()
    chain = self._ancestors(index, session_id)
    closed = [a.id for a in chain if self._facts_of(a.id).task_state != "open"]
    if closed:
      raise TaskConflictError([f"closed ancestor task(s): {', '.join(closed)}"])
    return chain

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
    live = self._sessions.load_chat_events_sync(session_id)
    archived_count = self._archived_event_count(session_id, live)
    if not archived_count:
      return live
    return [*self._load_archived_events(session_id, archived_count), *live]

  def _archived_event_count(self, session_id: str, live: list[dict]) -> int:
    total = self._sessions.get_chat_event_count_sync(session_id)
    return max(0, total - len(live))

  def _load_archived_events(self, session_id: str, count: int) -> list[dict]:
    events, _has_more = self._sessions.load_chat_events_range(session_id, 0, count)
    return events

  def _facts_of(self, session_id: str) -> _TaskFacts:
    """The session's folded task/run facts over the full history (suffix-memoized).

    The memo rides the chat-events cache's list identity (append-only growth or
    wholesale replacement) plus the archived extent: token streaming extends
    the fold by its suffix, and a rotation re-keys the archived half. A
    task_imported fact in the suffix moves the input boundary, so the suffix
    fold that sees one restarts from the whole history.
    """
    live = self._sessions.load_chat_events_sync(session_id)
    archived_count = self._archived_event_count(session_id, live)
    cached = self._facts_memo.get(session_id)
    if cached is not None and cached[0] is live and cached[1] == archived_count:
      facts = cached[2]
    else:
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
    """The task's derived lifecycle state without a caller-held index."""
    return self._facts_of(session_id).task_state

  def task_state_of(self, index: "_TreeIndex", session_id: str) -> str:
    return self._facts_of(session_id).task_state

  def work_state_of(self, index: "_TreeIndex", session_id: str) -> WorkState:
    """idle | running | waiting | attention, from CURRENT unresolved facts.

    An active run wins, then an unresolved failure, then waiting work. A
    failed or interrupted run draws attention only while it stands
    unresolved: a successful authorized retry (the retry_of_run_id chain)
    resolves the older failure instead of leaving attention forever.
    """
    facts = self._facts_of(session_id)
    runs = self.runs.list_run_records_sync(session_id)
    events = self.runs.load_events_sync(session_id)
    host_boot = self._host_boot_time()
    superseded: set[str] = set()
    for run in runs:
      if facts.run_outcomes.get(run.id) == "success":
        target = run.retry_of_run_id
        while target is not None and target not in superseded:
          superseded.add(target)
          target = next((r.retry_of_run_id for r in runs if r.id == target), None)
    verdicts: list[str] = []
    for run in runs:
      outcome = facts.run_outcomes.get(run.id)
      if outcome is not None:
        if outcome in ("failed", "interrupted") and run.id not in superseded:
          verdicts.append("attention")
        continue
      alive = self.runs.run_is_active(run, [], host_boot)
      if run.pid is None:
        if self.runs.stop_requested(events, run.id):
          continue  # a stopped queued run is resolved-by-request, not waiting work
        verdicts.append("waiting")  # queued: retains its inputs for later dispatch
      elif alive:
        verdicts.append("running")
      else:
        verdicts.append("attention")  # launched, exit observed by nobody yet
    for state in ("running", "attention", "waiting"):
      if state in verdicts:
        return state  # type: ignore[return-value]
    return "idle"

  def _host_boot_time(self) -> datetime:
    from src.core.runs import read_host_boot_time
    return read_host_boot_time()

  def _archived_facts_based(self, meta: SessionMetadata, facts: _TaskFacts) -> bool:
    """Archive visibility from a pre-folded fact set (the index build's form)."""
    if meta.presentation == "hidden":
      return True
    if meta.presentation == "shown":
      return False
    if meta.status == SessionStatus.ARCHIVED:
      return True  # a legacy archived preference stays an archive preference
    if facts.task_state != "completed":
      # Failed, blocked, cancelled, and open tasks stay visible; cancelled ones
      # until the user explicitly hides them.
      return False
    completed = [c for c in facts.close_events if c.get("outcome") == "completed"]
    if not completed:
      return False
    close = completed[-1]
    recipient = close.get("report_to")
    if not recipient:
      return True  # a root task archives immediately on its own success
    parent_facts = self._facts_of(str(recipient))
    return (meta.id, str(close.get("id"))) in parent_facts.delivered_reports

  def archived_of(self, index: "_TreeIndex", meta: SessionMetadata) -> bool:
    """Archive visibility: the explicit preference, or auto after successful receipt.

    presentation=auto archives a successful task once its parent receipt is on
    disk (a root immediately on success); shown keeps it visible; hidden is an
    explicit user collapse. Reopened nodes are open again, so they unarchive.
    """
    return self._archived_facts_based(meta, self._facts_of(meta.id))

  def session_row(self, index: "_TreeIndex", session_id: str) -> SessionRow:
    meta = self._index_meta(index, session_id)
    child_ids = self._children_of(index, session_id)
    descendants = self._descendants(index, session_id)
    open_count = sum(1 for d in descendants if self.task_state_of(index, d) == "open")
    attention_count = sum(1 for d in descendants if self.work_state_of(index, d) == "attention")
    return SessionRow(
        id=meta.id,
        name=meta.name,
        profile=meta.profile,
        task_parent_id=meta.task_parent_id,
        task_state=self.task_state_of(index, session_id),  # type: ignore[arg-type]
        work_state=self.work_state_of(index, session_id),
        archived=self.archived_of(index, meta),
        child_count=len(child_ids),
        open_descendant_count=open_count,
        attention_descendant_count=attention_count,
    )

  async def session_detail(self, session_id: str) -> dict:
    """The session detail projection: existing metadata plus the derived task fields."""
    meta = await self.load_meta(session_id)
    self._require_task(meta, session_id)
    assert meta is not None
    index = await self._get_index()
    # Re-read through the index so the detail and the tree agree on one projection.
    indexed = index.metas.get(session_id)
    if indexed is None:
      raise TaskInvalidError(f"session {session_id} is not a task-tree node (no profile)")
    ancestors = self._ancestors(index, session_id)
    payload = indexed.model_dump(mode="json")
    payload.update({
        "task_state": self.task_state_of(index, session_id),
        "work_state": self.work_state_of(index, session_id),
        "archived": self.archived_of(index, indexed),
        "ancestors": [AncestorRef(id=a.id, name=a.name).model_dump() for a in ancestors],
    })
    return payload

  # ------------------------------------------------------------------
  # Create
  # ------------------------------------------------------------------

  async def create_task(
      self,
      *,
      request_id: str,
      task_parent_id: str | None,
      profile: str | None,
      task: TaskSpec | None,
      name: str | None,
      backend: str | None,
      caller: object,
  ) -> SessionMetadata:
    """Create one task node; a replayed request returns the original product.

    The node id is (parent, request_id)-stable. Metadata plus the task_created
    fact are written into a temp directory and published with one rename, so a
    crash leaves either no node or a complete one.
    """
    if not request_id:
      raise TaskInvalidError("request_id is required for task creation")
    if profile not in ("manager", "worker"):
      raise TaskInvalidError("profile must be 'manager' or 'worker'")
    task_id = stable_task_id(task_parent_id, request_id)
    async with self.control_lock:
      existing = await self.load_meta(task_id)
      if existing is not None:
        # A replayed operation returns its original product only to a caller
        # authorized for that same create — the replay is never an
        # authorization bypass.
        if isinstance(caller, CallerIdentity) and not caller.is_operator:
          parent_meta = await self.load_meta(task_parent_id) if task_parent_id is not None else None
          parent_meta = parent_meta if parent_meta is not None and parent_meta.profile is not None else None
          await self._authorize_agent_worker_creation(caller, profile, task_parent_id, parent_meta)
        return existing
      parent_meta: SessionMetadata | None = None
      # Caller scope first: an agent's 403 must not depend on the target's shape.
      if isinstance(caller, CallerIdentity) and not caller.is_operator:
        if task_parent_id is not None:
          parent_meta = await self.load_meta(task_parent_id)
          parent_meta = parent_meta if parent_meta is not None and parent_meta.profile is not None else None
        await self._authorize_agent_worker_creation(caller, profile, task_parent_id, parent_meta)
      if task_parent_id is not None:
        parent_meta = await self.load_meta(task_parent_id)
        self._require_task(parent_meta, task_parent_id)
        assert parent_meta is not None
        if parent_meta.profile != "manager":
          raise TaskInvalidError(f"parent task {task_parent_id} is not a manager")
        index = await self._get_index()
        parent_state = self.task_state_of(index, task_parent_id)
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
          parent_meta=parent_meta,
          actor=_create_actor_for(caller),
      )
    self._invalidate_index()
    # The publish rename took the node out from under any cached entry.
    self._sessions._invalidate_cache(task_id)
    fresh = await self.load_meta(task_id)
    assert fresh is not None
    return fresh

  async def _authorize_agent_worker_creation(
      self,
      caller: "object",
      profile: str,
      task_parent_id: str | None,
      parent_meta: SessionMetadata | None,
  ) -> None:
    """An agent caller may create only a worker directly under its own open manager task."""
    assert isinstance(caller, CallerIdentity)
    claims = caller.claims
    assert claims is not None
    if (profile != "worker" or task_parent_id != claims.session_id or parent_meta is None or
            parent_meta.profile != "manager"):
      raise TaskForbiddenError(
          "an agent may only create a worker task directly under its own open manager task")
    # The caller's own manager task must carry the authorization: the
    # nearest-real-user-ancestor gate (takeoff_gate) decides.
    await self.check_task_authorization(claims.session_id)

  async def _require_open_ancestry_from_index(self, index: "_TreeIndex", session_id: str) -> None:
    chain = self._ancestors(index, session_id)
    closed = [a.id for a in chain if self._facts_of(a.id).task_state != "open"]
    if closed:
      raise TaskConflictError([f"closed ancestor task(s): {', '.join(closed)}"])

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
      parent_meta: SessionMetadata | None,
      actor: str,
  ) -> SessionMetadata:
    sessions_dir = self._cfg.sessions_dir
    sessions_dir.mkdir(parents=True, exist_ok=True)
    final_dir = sessions_dir / task_id
    temp_dir = sessions_dir / f".task-{task_id}-{os.getpid()}-{uuid4_hex()}.tmp"
    meta = SessionMetadata(
        id=task_id,
        name=name or default_task_name(task, profile),
        schema_version=2,
        profile=profile,  # type: ignore[arg-type]
        task=task,
        task_parent_id=task_parent_id,
        backend=backend or (parent_meta.backend if parent_meta else "") or self._cfg.backends.options[0].id,
    )
    try:
      (temp_dir / "data").mkdir(parents=True)
      (temp_dir / "threads").mkdir()
      # The creation fact is written into the temp node itself, so metadata and
      # the event log publish together with the one rename; post-publication
      # facts go through the sink.
      created_event = build_control_event(
          ET.TASK_CREATED,
          actor=actor,
          source_session_id=task_id,
          request_id=request_id,
          task_parent_id=task_parent_id,
          task_spec_hash=canonical_task_spec_hash(task),
      )
      await append_ndjson(temp_dir / "data" / "chat_events.jsonl", created_event)
      meta.created_by_event = EventRef(session_id=task_id, event_id=str(created_event["id"]))
      await asyncio.to_thread(
          atomic_write_text, temp_dir / "metadata.json",
          meta.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS))
      try:
        os.replace(temp_dir, final_dir)
      except OSError:
        # Lost a create race for the same stable id: the original product exists.
        existing = self._read_metadata_file(final_dir / "metadata.json")
        if existing is None:
          raise
        return existing
      return meta
    finally:
      if temp_dir.exists():
        shutil.rmtree(temp_dir, ignore_errors=True)

  @staticmethod
  def _read_metadata_file(path) -> SessionMetadata | None:
    try:
      return SessionMetadata.model_validate_json(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
      return None

  async def check_task_authorization(self, session_id: str, now: datetime | None = None) -> str:
    """The nearest-real-user-ancestor gate for a v2 task caller (takeoff_gate).

    Every ancestor must be open and the calling node a manager; the first node
    holding a real user instruction is where the legacy time-window rules
    apply, and a failure there blocks without borrowing from higher ancestors.
    """
    from src.core.takeoff_gate import check_takeoff_gate_for_task
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
      return self.task_state_of(index, sid)

    return check_takeoff_gate_for_task(
        session_id,
        load_events=self.events.load_events,
        task_meta_of=meta_of,
        task_state_of=state_of,
        now=now,
    )

  async def record_scheduled_fire(
      self,
      session_id: str,
      *,
      last_scheduled_run: str | None = None,
      cron: str | None = None,
      last_run_status: str | None = None,
  ) -> SessionMetadata:
    """The scheduler's per-fire bookkeeping on one bound node.

    The node is re-read under the control lock and only the scheduling fields
    named by the caller are written, so a concurrent task edit (name, spec,
    prompts, pause) between the scheduler's earlier load and this write is
    preserved instead of being overwritten by a stale SessionMetadata
    snapshot. This is the metadata owner's single entry for cron bookkeeping.
    """
    if last_scheduled_run is None and cron is None and last_run_status is None:
      raise TaskInvalidError("record_scheduled_fire requires at least one scheduling field")
    async with self.control_lock:
      meta = await self.load_meta(session_id)
      self._require_task(meta, session_id)
      assert meta is not None
      if last_scheduled_run is not None:
        meta.last_scheduled_run = last_scheduled_run
      if cron is not None:
        meta.last_scheduled_cron = cron
      if last_run_status is not None:
        meta.last_run_status = last_run_status
      meta.updated_at = utc_now()
      await self._save_meta(meta)
      return meta

  async def create_retry(self, session_id: str, request_id: str, original_run_id: str) -> dict:
    """Create one retry run on the open task; a replayed request returns the same run.

    The retry pins the task's current spec text (later edits keep the old
    versions pinned to their own runs) and inherits the retried run's
    execution context; the previous run's evidence is preserved untouched.
    """
    async with self.control_lock:
      await self._get_index()
      meta = await self.load_meta(session_id)
      self._require_task(meta, session_id)
      state = self.task_state_of(self._index[0], session_id)
      if state != "open":
        raise TaskConflictError([f"task {session_id} is {state}; only open tasks accept retries"])
      original = await self.runs.get_run(session_id, original_run_id)
      if original is None:
        raise TaskNotFoundError(f"run {original_run_id} not found in session {session_id}")
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
      )
    return {"session_id": session_id, "run_id": run.id}

  # ------------------------------------------------------------------
  # Patch
  # ------------------------------------------------------------------

  async def patch_task(self, session_id: str, req: PatchSessionTaskRequest, *, caller: object) -> SessionMetadata:
    """Apply one v2 metadata mutation with its structural guards (operator-only)."""
    if not isinstance(caller, CallerIdentity) or not caller.is_operator:
      raise TaskForbiddenError("task metadata mutations require operator credentials")
    async with self.control_lock:
      await self._get_index()
      meta = await self.load_meta(session_id)
      self._require_task(meta, session_id)
      assert meta is not None
      fs = req.model_fields_set
      structural = bool(fs & {"task", "profile", "task_parent_id"})
      blockers = self._structural_blockers(session_id) if structural else []
      if "profile" in fs and req.profile is not None and req.profile != meta.profile:
        if req.profile == "worker" and self._children_count(session_id) > 0:
          blockers.append("demotion to worker requires a task with no child tasks")
      if "task_parent_id" in fs and req.task_parent_id != meta.task_parent_id:
        blockers.extend(await self._reparent_blockers(session_id, req.task_parent_id))
      if blockers:
        raise TaskConflictError(sorted(set(blockers)))
      if "name" in fs and req.name is not None:
        meta.name = req.name
      if "presentation" in fs and req.presentation is not None:
        meta.presentation = req.presentation
      if "automation_paused" in fs and req.automation_paused is not None:
        meta.automation_paused = req.automation_paused
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
      return meta

  async def set_presentation(self, session_id: str, presentation: str) -> SessionMetadata:
    """The legacy archive/unarchive entries' v2 form: one explicit preference.

    This is a display preference only — the task's open/closed facts are
    untouched, so collapsing can never silently close a task and uncollapsing
    can never silently reopen one.
    """
    if presentation not in ("auto", "shown", "hidden"):
      raise TaskInvalidError(f"presentation must be auto, shown, or hidden (got {presentation!r})")
    async with self.control_lock:
      await self._get_index()
      meta = await self.load_meta(session_id)
      self._require_task(meta, session_id)
      assert meta is not None
      if meta.presentation != presentation:
        meta.presentation = presentation  # type: ignore[assignment]
        await self._save_meta(meta)
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
      if self.task_state_of(index, new_parent_id) != "open":
        blockers.append(f"reparent target {new_parent_id} is {self.task_state_of(index, new_parent_id)}")
      chain_ids = [a.id for a in self._ancestors(index, new_parent_id)]
      if session_id == new_parent_id or session_id in chain_ids:
        blockers.append(f"reparent target {new_parent_id} is inside {session_id}'s own subtree")
      else:
        closed = [a.id for a in self._ancestors(index, new_parent_id)
                  if self._facts_of(a.id).task_state != "open"]
        if closed:
          blockers.append(f"closed ancestor task(s): {', '.join(closed)}")
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
    """Store the rule body immutable, atomically swap the reference, record prompt_changed."""
    attr = f"{scope}_prompt_ref"
    previous_ref: str | None = getattr(meta, attr)
    new_ref: str | None = None
    if body is not None:
      new_ref = await asyncio.to_thread(self._store_prompt_body, body)
    if new_ref == previous_ref:
      return
    setattr(meta, attr, new_ref)
    await self._save_meta(meta)
    await self.events.append(session_id, build_control_event(
        ET.PROMPT_CHANGED,
        actor=ACTOR_USER,
        source_session_id=session_id,
        scope=scope,
        previous_ref=previous_ref,
        new_ref=new_ref,
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
      rows_all = [r for r in rows_all if not r.archived]
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
    if meta.profile is None:
      blockers = self._legacy_deletion_blockers(index, session_id)
    else:
      blockers = self._deletion_blockers_locked(index, session_id)
    return blockers

  def _legacy_deletion_blockers(self, index: "_TreeIndex", session_id: str) -> list[str]:
    """The v1 check set (children via the flat index, runs, triggers, aliases)."""
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
    for old_id in self.aliases.old_ids_for(session_id):
      blockers.append(f"referenced by session alias for old id {old_id}")
    return blockers

  def _deletion_blockers_locked(self, index: "_TreeIndex", session_id: str) -> list[str]:
    """The v2 empty/unreferenced rule, evaluated under the control lock.

    No children, runs, triggers, aliases, or any other saved structured
    reference (origin/created-by/parent/successor pointers from other
    records, child reports another log holds), and no preserved conversation
    or evidence beyond the creation fact itself.
    """
    blockers = self._legacy_deletion_blockers(index, session_id)
    facts = self._facts_of(session_id)
    substance = [
        e for e in facts.events_by_id.values() if e.get("type") != ET.TASK_CREATED]
    if substance:
      kinds = sorted({str(e.get("type")) for e in substance})
      blockers.append(f"has preserved conversation/evidence: {', '.join(kinds)}")
    for other_id, other in index.metas.items():
      if other_id == session_id or other.profile is None:
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
    if not isinstance(caller, CallerIdentity) or not caller.is_operator:
      raise TaskForbiddenError("permanent delete requires operator credentials")
    async with self.control_lock:
      index = await self._get_index(force=True)
      self._index_meta(index, session_id)
      meta = index.metas[session_id]
      if meta.profile is not None:
        blockers = self._deletion_blockers_locked(index, session_id)
        if blockers:
          raise TaskConflictError(sorted(set(blockers)))
      result = await self._sessions.delete_session_permanently(session_id)
    if result:
      self._invalidate_index()
      self._facts_memo.pop(session_id, None)
    return result


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


def uuid4_hex() -> str:
  from uuid import uuid4
  return uuid4().hex


def default_task_name(task: TaskSpec | None, profile: str) -> str:
  """A readable fallback name: the goal's first line, else the profile."""
  if task is not None and task.goal.strip():
    first_line = task.goal.strip().splitlines()[0]
    return first_line[:80]
  return f"New {profile} task"


def jsonable_row(row: SessionRow) -> dict:
  return row.model_dump()


def _encode_tree_cursor(revision: str, key: tuple[datetime, str]) -> str:
  """Revision-bound keyset cursor: base64url JSON of (revision, created_at, id)."""
  payload = {"r": revision, "a": key[0].isoformat(), "i": key[1]}
  return base64.urlsafe_b64encode(orjson.dumps(payload)).rstrip(b"=").decode("ascii")


def _decode_tree_cursor(cursor: str) -> tuple[str, tuple[datetime, str]]:
  """Decode one tree cursor; a malformed value fails loud (API: 400)."""
  try:
    payload = orjson.loads(base64.urlsafe_b64decode(cursor + "=" * (-len(cursor) % 4)))
    key = (ensure_utc(datetime.fromisoformat(payload["a"])), payload["i"])
    return payload["r"], key
  except (ValueError, KeyError, TypeError) as e:
    raise TaskInvalidError(f"malformed tree page cursor: {cursor!r}") from e


def ensure_utc(value: datetime) -> datetime:
  from src.core.models import ensure_utc as _ensure
  return _ensure(value)
