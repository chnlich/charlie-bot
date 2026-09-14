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
- Input delivery (``session_dispatch.py``) and completion guards
  (``task_completion.py``) join the control/event seams in their own delivery
  stage; the seams are the sink, the pending-input blocker hook, and the
  derived-fact folds.
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
from src.core.control_events import (
    ACTOR_AGENT,
    ACTOR_USER,
    ControlEventSink,
    build_control_event,
    sha256_hex,
    stable_task_id,
)
from src.core.ndjson import append_ndjson
from src.core.models import (
    AncestorRef,
    EventRef,
    PatchSessionTaskRequest,
    RunRecord,
    SessionMetadata,
    SessionRow,
    SessionStatus,
    TaskSpec,
    WorkState,
    utc_now,
)
from src.core.config import CharlieBotConfig
from src.core.json_utils import atomic_write_text
from src.core.run_token import CallerIdentity
from src.core.runs import RunStore
from src.core.session_aliases import SessionAliasStore
from src.core.sessions import _TRANSIENT_METADATA_FIELDS, SessionManager

if TYPE_CHECKING:
  from src.core.takeoff_gate import DelegationBlockedError

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
  """The derived facts one session's event stream folds to (suffix-memoized)."""
  task_state: str = "open"
  run_outcomes: dict[str, str] = field(default_factory=dict)
  covered: int = 0


class TaskTreeManager:
  """The task-tree record owner wired over one SessionManager."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager) -> None:
    self._cfg = cfg
    self._sessions = session_mgr
    self.control_lock = asyncio.Lock()
    self.events = ControlEventSink(session_mgr)
    self.aliases = SessionAliasStore(cfg.sessions_dir)
    self.runs = RunStore(cfg, self.control_lock, self.events, self.aliases)
    # Seam for the input-delivery stage: a callable returning the pending-input
    # blockers of one session ([] until session_dispatch.py owns it).
    self.pending_input_blockers: Callable[[str], list[str]] | None = None
    self._index: tuple[_TreeIndex, float] | None = None
    self._facts_memo: dict[str, tuple[list[dict], _TaskFacts]] = {}
    self._prompt_bodies_dir = cfg.charliebot_home / PROMPT_BODIES_DIR_NAME

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
    self._index = None  # any metadata write may move the projection inputs

  # ------------------------------------------------------------------
  # Tree index (rebuildable)
  # ------------------------------------------------------------------

  async def _get_index(self) -> "_TreeIndex":
    now = time.monotonic()
    if self._index is not None and now - self._index[1] < _TREE_INDEX_TTL_SECONDS:
      return self._index[0]
    index = await asyncio.to_thread(self._build_index_sync)
    self._index = (index, now)
    return index

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
      structural.append(f"{sid}|{meta.task_parent_id or ''}|{meta.profile}|{meta.presentation}|{meta.status.value}")
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
    closed = [a.id for a in chain if self._facts_of(index, a.id).task_state != "open"]
    if closed:
      raise TaskConflictError([f"closed ancestor task(s): {', '.join(closed)}"])
    return chain

  # ------------------------------------------------------------------
  # Derived facts (rebuilt from durable facts, suffix-memoized)
  # ------------------------------------------------------------------

  def _facts_of(self, index: "_TreeIndex", session_id: str) -> _TaskFacts:
    """The session's task/run facts, folding only the event-stream suffix since the last call.

    The memo rides the chat-events cache's list identity (append-only growth or
    wholesale replacement), so token streaming extends the fold by its suffix
    and never re-walks the covered prefix.
    """
    events = self._sessions.load_chat_events_sync(session_id)
    cached = self._facts_memo.get(session_id)
    if cached is not None and cached[0] is events:
      facts = cached[1]
    else:
      facts = _TaskFacts()
    start = facts.covered
    for event in events[start:]:
      etype = event.get("type")
      if etype == ET.TASK_CLOSED:
        facts.task_state = str(event.get("outcome") or "completed")
      elif etype == ET.TASK_REOPENED:
        facts.task_state = "open"
      elif etype == ET.RUN_FINISHED:
        run_id = event.get("run_id")
        if isinstance(run_id, str):
          facts.run_outcomes[run_id] = str(event.get("outcome"))
    facts.covered = len(events)
    self._facts_memo[session_id] = (events, facts)
    return facts

  def task_state_of(self, index: "_TreeIndex", session_id: str) -> str:
    return self._facts_of(index, session_id).task_state

  def work_state_of(self, index: "_TreeIndex", session_id: str) -> WorkState:
    """idle | running | waiting | attention, rebuilt from run records plus terminal facts."""
    meta = self._index_meta(index, session_id)
    facts = self._facts_of(index, session_id)
    runs = self.runs.list_run_records_sync(session_id)
    host_boot = self._host_boot_time()
    verdicts: list[str] = []
    for run in runs:
      outcome = facts.run_outcomes.get(run.id)
      alive = self.runs.run_is_active(run, [], host_boot)
      if outcome is None:
        if run.pid is None:
          verdicts.append("waiting")  # queued: retains inputs for later dispatch
        elif alive:
          verdicts.append("running")
        else:
          verdicts.append("attention")  # launched, exit observed by nobody yet
      elif alive:
        verdicts.append("running")
      elif outcome in ("failed", "interrupted"):
        verdicts.append("attention")
      else:
        verdicts.append("idle")
    _ = meta  # reserved: automation_paused gates *dispatch*, not the work-state display
    for state in ("running", "attention", "waiting"):
      if state in verdicts:
        return state  # type: ignore[return-value]
    return "idle"

  def _host_boot_time(self) -> datetime:
    from src.core.runs import read_host_boot_time
    return read_host_boot_time()

  def archived_of(self, meta: SessionMetadata) -> bool:
    """Archive visibility: the legacy status flag or the user's hidden preference."""
    return meta.status == SessionStatus.ARCHIVED or meta.presentation == "hidden"

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
        archived=self.archived_of(meta),
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
        "archived": self.archived_of(indexed),
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
        return existing
      parent_meta: SessionMetadata | None = None
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
      if isinstance(caller, CallerIdentity) and not caller.is_operator:
        await self._authorize_agent_worker_creation(caller, task_parent_id, parent_meta)
      meta = await self._publish_new_task(
          task_id=task_id,
          request_id=request_id,
          task_parent_id=task_parent_id,
          profile=profile,
          task=task,
          name=name,
          backend=backend,
          parent_meta=parent_meta,
          actor=ACTOR_USER if (isinstance(caller, CallerIdentity) and caller.is_operator) else ACTOR_AGENT,
      )
    self._index = None
    # The publish rename took the node out from under any cached entry.
    self._sessions._invalidate_cache(task_id)
    fresh = await self.load_meta(task_id)
    assert fresh is not None
    return fresh

  async def _authorize_agent_worker_creation(
      self,
      caller: "object",
      task_parent_id: str | None,
      parent_meta: SessionMetadata | None,
  ) -> None:
    """An agent caller may create only a worker directly under its own open manager task."""
    assert isinstance(caller, CallerIdentity)
    claims = caller.claims
    assert claims is not None
    if profile_guard_failed(claims, task_parent_id, parent_meta):
      raise TaskForbiddenError(
          "an agent may only create a worker task directly under its own open manager task")
    # The caller's own manager task must carry the authorization: the
    # nearest-real-user-ancestor gate (takeoff_gate) decides.
    await asyncio.to_thread(self.check_task_authorization, claims.session_id)

  async def _require_open_ancestry_from_index(self, index: "_TreeIndex", session_id: str) -> None:
    chain = self._ancestors(index, session_id)
    closed = [a.id for a in chain if self._facts_of(index, a.id).task_state != "open"]
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

  def check_task_authorization(self, session_id: str, now: datetime | None = None) -> str:
    """The nearest-real-user-ancestor gate for a v2 task caller (takeoff_gate).

    Every ancestor must be open and the calling node a manager; the first node
    holding a real user instruction is where the legacy time-window rules
    apply, and a failure there blocks without borrowing from higher ancestors.
    """
    from src.core.takeoff_gate import check_takeoff_gate_for_task
    if self._index is None:
      raise RuntimeError("tree index must be built before authorization checks")
    index = self._index[0]

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
        load_events=self._events.load_events,
        task_meta_of=meta_of,
        task_state_of=state_of,
        now=now,
    )

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
      run = await self.runs.create_retry_run(
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
                  if self._facts_of(index, a.id).task_state != "open"]
        if closed:
          blockers.append(f"closed ancestor task(s): {', '.join(closed)}")
    # The moving subtree itself must be idle: every node in it, self included.
    subtree = [session_id, *self._descendants(index, session_id)]
    for sid in subtree:
      blockers.extend(f"{sid}: {b}" for b in self._structural_blockers(sid))
    # Historical child_report recipients (report_to) are fixed facts of past
    # close events and are never rewritten by a move; the seam for *pending*
    # (undelivered) reports is the input-delivery stage's.
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
    """Permanent delete requires an empty task: no children, runs, triggers or references."""
    index = await self._get_index()
    self._index_meta(index, session_id)
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


# ---------------------------------------------------------------------------
# Module helpers
# ---------------------------------------------------------------------------


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
