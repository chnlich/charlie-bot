"""The task-tree execution adapter: binds v2 Runs to the real backend processes.

This module is the execution-glue owner of the session task tree. It holds the
one :class:`TaskExecutionAdapter` the application installs as the
:class:`~src.core.session_dispatch.TaskInputDispatcher` executor, plus the
common launch/resume interfaces the later controller stages (improve, cron,
delayed triggers) and startup recovery consume.

Ownership boundaries it never crosses:

- Input admission, dedup and batch claims stay in
  :mod:`src.core.session_dispatch`; run records, terminal facts and stop
  requests stay in :mod:`src.core.runs`; task metadata and tree relations stay
  in :mod:`src.core.task_sessions`; completion/evidence policy stays in
  :mod:`src.core.task_completion`. This module only binds a registered Run to
  the existing process harnesses — the master queue (``src.agents.master_cc``)
  for manager turns, the Worker/backend adapters (``src.agents.worker``) for
  work and review — and lands their outcomes through those owners.
- The one short control write lock is held only across durable reservation
  and identity writes. Backend, git, network and model work run outside it.
- A v2 Run is the sole new execution record: no second
  ``SessionMetadata.master_run`` and no second writable ``ThreadMetadata``
  file is ever written for a v2 launch. Legacy v1 sessions keep their
  existing paths untouched.

Serialization evidence: concurrent dispatch calls reserve the consumer under
the control lock — a fresh consumer binds the exact pending batch through
``claim_input_batch``, so the second caller's claim comes back empty and it
never spawns; a queued Run is launched through the in-process launch guard so
two dispatch calls cannot both start it. The spawned process identity
(``pid`` + ``pid_start``) lands on the Run before any call from its credential
is accepted (:meth:`RunStore.record_launch` runs inside the backend's
on_spawn callback, and the caller-identity dependency requires both fields).
"""

from __future__ import annotations

import asyncio
import dataclasses
import errno
import functools
import json
import shutil
import time
import uuid
from collections.abc import Awaitable, Callable
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

from src.agents.worker import QuotaExhaustedError, Worker
from src.core import claude_accounts, claude_relay, git, review, runs, task_prompts
from src.core import event_types as ET
from src.core.chat_events import chat_events_path
from src.core.config import CharlieBotConfig, configured_access_key
from src.core.constants import SESSION_ID_ENV_VAR, BackendType
from src.core.control_events import (
    ACTOR_SYSTEM,
    build_control_event,
    sha256_hex,
    stable_run_id,
    stable_withheld_event_id,
)
from src.core.log_once import LazyStructlogLogger
from src.core.models import (
    BackendOption,
    ClaudeAccount,
    RunRecord,
    SessionMetadata,
    TaskType,
    utc_now_iso,
)
from src.core.run_token import RUN_TOKEN_ENV, RunTokenClaims, sign_run_token
from src.core.runs import RUN_EVENTS_NAME, RunNotFoundError, run_not_found_in_task_text, scan_result_exit
from src.core.session_dispatch import child_report_text
from src.core.sessions import SessionManager, backend_switch_reset_reason, context_reset_note
from src.core.spawner_backends import resolve_backend_option
from src.core.task_prompts import WORKER_KINDS, PromptSnapshot, TaskPromptError
from src.core.task_sessions import (
    TaskConflictError,
    TaskInvalidError,
    TaskNotFoundError,
    canonical_task_spec_text,
)

if TYPE_CHECKING:
  from src.core.task_sessions import TaskTreeManager

log = LazyStructlogLogger()


@dataclasses.dataclass
class RunWorkerBinding:
  """The Run-backed worker identity a v2 launch hands to :class:`Worker`.

    Duck-types the ``ThreadMetadata`` fields the Worker reads (id, session_id,
    pid, pid_start, claude_session_id) without ever materializing a legacy
    thread record: the pid/pid_start pair is persisted onto the Run by the
    adapter's ``on_spawned`` callback, not onto a thread file.
    """

  id: str
  session_id: str
  pid: int | None = None
  pid_start: str | None = None
  claude_session_id: str | None = None


@dataclasses.dataclass(frozen=True)
class LaunchSettlement:
  """The settled launch/wait verdict one controller-owned Run reached.

    ``outcome`` is the durable terminal outcome of a Run that executed;
    ``withheld`` is the actual reason no process will ever start for it (the
    launch precondition failed in the registration-to-launch interval). The
    two are mutually exclusive, and neither ever fabricates a result.
    """

  outcome: str | None = None
  withheld: str | None = None


LAUNCH_STARTED = "started"

# The end-landing retry's cadence: the first round runs immediately, later
# rounds every interval, with no round limit. A module constant so tests
# shorten it instead of sleeping 30 s.
RUN_END_LANDING_RETRY_INTERVAL_SECONDS = 30.0

_OUT_OF_SPACE_ERRNOS = frozenset({errno.ENOSPC, errno.EDQUOT})


def is_out_of_space_error(exc: BaseException) -> bool:
  """Whether *exc* is a disk-full error: an OSError with errno ENOSPC or EDQUOT
  on the exception itself or anywhere on its ``__cause__``/``__context__`` chain.

  The one owner of the predicate every end-landing retry handover reads. Only
  out-of-space errors enter the retry — they clear once space is freed — while
  any other error keeps the immediate landing, so a code defect in the end path
  never retries forever.
  """
  seen: set[int] = set()
  current: BaseException | None = exc
  while current is not None and id(current) not in seen:
    seen.add(id(current))
    if isinstance(current, OSError) and current.errno in _OUT_OF_SPACE_ERRNOS:
      return True
    current = current.__cause__ or current.__context__
  return False


def free_disk_gib(path: Path) -> float:
  """Free bytes on the filesystem holding *path*, in GiB.

  A not-yet-created path (a fresh worktree root) probes the nearest existing
  ancestor — the filesystem that would hold it.
  """
  probe = path
  while not probe.exists():
    parent = probe.parent
    if parent == probe:
      raise FileNotFoundError(f"no existing ancestor to probe for {path}")
    probe = parent
  return shutil.disk_usage(probe).free / (1024**3)


def compose_input_prompt(events: list[dict]) -> tuple[str, list[dict]]:
  """The manager-turn prompt body from its exact durable input batch.

    Real user input rides verbatim (its event is the durable fact the Run
    acknowledges — no second synthetic USER copy is persisted); relays,
    child reports and scheduled triggers keep their own typed framing so the
    manager sees the provenance the event carries. Attachments ride the turn.
    """
  parts: list[str] = []
  uploads: list[dict] = []
  for event in events:
    event_type = event.get("type")
    content = str(event.get("content") or "")
    if event_type == ET.USER:
      parts.append(content)
    elif event_type == ET.AGENT_MESSAGE:
      parts.append(
          f"[Message from session {event.get('from_session_name') or event.get('from_session') or 'unknown'}] "
          f"{content}")
    elif event_type == ET.CHILD_REPORT:
      parts.append(child_report_text(event))
    elif event_type == ET.SCHEDULED_TRIGGER:
      parts.append(f"[Scheduled trigger] {content}")
    else:
      parts.append(content)
    uploads.extend(event.get("uploaded_files") or [])
  return "\n\n".join(part for part in parts if part), uploads


def resolve_launch_overlay(option: BackendOption) -> tuple[str | None, bool]:
  """The three-state overlay judgment a v2 launch shares with the v1 wake path.

    Returns (overlay_name_or_None, declared). None + declared=False means
    undeclared (alert); "none" is normalized to (None, True) — explicitly no
    overlay, silent; any other string names the overlay file.
    """
  overlay = option.prompt_overlay
  if overlay is None:
    return None, False
  if overlay == "none":
    return None, True
  return overlay, True


def capture_prompt_chain(
    tree: TaskTreeManager,
    index: object,
    meta: SessionMetadata,
) -> tuple[tuple[tuple[str, str | None], ...], str | None]:
  """Subtree refs root → this node (inclusive) plus this node's own rule ref.

    The subtree scope is THIS NODE AND ITS DESCENDANTS: the chain ends with
    ``(meta.id, meta.subtree_prompt_ref)`` so the node's own subtree rule
    applies to its own next context, not only to its descendants'. Ancestor
    node rules and sibling rules never enter. Reads the tree index the
    caller's control-lock hold just built; refs are the metadata-owned
    fingerprints the recheck compares.
    """
  chain: list[tuple[str, str | None]] = []
  for ancestor in reversed(tree._ancestors(index, meta.id)):  # root → parent
    ancestor_meta = index.metas.get(ancestor.id)
    chain.append((ancestor.id, ancestor_meta.subtree_prompt_ref if ancestor_meta else None))
  chain.append((meta.id, meta.subtree_prompt_ref))
  return tuple(chain), meta.node_prompt_ref


async def assemble_coherent_snapshot(
    cfg: CharlieBotConfig,
    tree: TaskTreeManager,
    meta: SessionMetadata,
    kind: str,
    option: BackendOption,
) -> tuple[PromptSnapshot, OSError | None, bool]:
  """One coherent assembly pass over the tree, templates, memory and rules.

    Ancestor relations and refs are captured under the control lock; the
    bodies, templates, host/overlay supplements and memory are read outside it.
    Before returning, the chain/refs are re-captured under the lock and the
    mutable sources are re-assembled: any change rebuilds from a coherent view
    (bounded passes, then a visible failure). The launch path commits the
    returned snapshot; the preview endpoint returns it as the next-start
    configuration — one assembly path for both, never a preview-only selector.
    """
  overlay, declared = resolve_launch_overlay(option)
  snapshot: PromptSnapshot | None = None
  overlay_error: OSError | None = None
  for _ in range(task_prompts._COHERENCE_PASSES):
    async with tree.control_lock:
      index = await tree._get_index()
      fresh_meta = await tree.load_meta(meta.id)
      if fresh_meta is None:
        raise TaskNotFoundError(f"task {meta.id} vanished during prompt assembly")
      chain, node_ref = capture_prompt_chain(tree, index, fresh_meta)
    built, err = await asyncio.to_thread(
        task_prompts.build_segments, cfg, fresh_meta, kind, overlay=overlay, chain=chain, node_ref=node_ref)
    candidate = task_prompts.assemble_snapshot(built)
    # Mutable-source fingerprint recheck: re-assemble and compare. Any
    # template/host/overlay/memory change between the two passes means no
    # coherent view existed yet.
    rebuilt, err2 = await asyncio.to_thread(
        task_prompts.build_segments, cfg, fresh_meta, kind, overlay=overlay, chain=chain, node_ref=node_ref)
    if candidate.to_json_dict() != task_prompts.assemble_snapshot(rebuilt).to_json_dict():
      continue
    async with tree.control_lock:
      index = await tree._get_index()
      fresh_meta = await tree.load_meta(meta.id)
      if fresh_meta is None:
        raise TaskNotFoundError(f"task {meta.id} vanished during prompt assembly")
      chain2, node_ref2 = capture_prompt_chain(tree, index, fresh_meta)
    if (chain2, node_ref2) != (chain, node_ref):
      continue  # a rule/ancestor moved: rebuild from the new view
    snapshot, overlay_error = candidate, (err or err2)
    break
  if snapshot is None:
    raise TaskPromptError(
        f"prompt sources for task {meta.id} did not settle across "
        f"{task_prompts._COHERENCE_PASSES} coherent-view attempts")
  return snapshot, overlay_error, declared


class TaskExecutionAdapter:
  """Binds registered Runs to the existing master/worker execution harnesses."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager, tree: TaskTreeManager) -> None:
    self._cfg = cfg
    self._sessions = session_mgr
    self._tree = tree
    # In-process launch guard: one execute task per run per process. The
    # durable (pid, pid_start) identity write is the cross-restart
    # backstop; this set closes the same-loop double-schedule window.
    self._launch_inflight: set[tuple[str, str]] = set()
    # Launch settlements: one future per scheduled launch, resolved when
    # the launch attempt settles — the process started, or a launch
    # precondition refused it (no terminal fact will ever arrive). The
    # sequence controllers await this barrier instead of polling forever
    # behind a withheld launch.
    self._launch_settlements: dict[tuple[str, str], asyncio.Future] = {}
    # Launch workspace boundary: when installed (the session-tree preview
    # entry point), every worktree-creating launch refuses a repo outside
    # the instance's own workspace dirs. None leaves production behavior
    # unchanged.
    self.launch_workspace_guard: Callable[[Path], None] | None = None
    # Background-follow registry, same shape as _launch_inflight: the (session,
    # run) pairs whose resume follow THIS process currently drives. Maintained
    # by resume_run — a worker follow leaves when its resume_run finishes, a
    # manager-turn follow only when its master-queue future resolves. The
    # end-landing retry's node reconcile pass skips these runs (boot reconcile
    # assumes no in-process follower; the retry runs while the server is live).
    self._resume_follows: set[tuple[str, str]] = set()
    # The end-landing retry tasks: at most one per node (session id -> task).
    self._landing_retries: dict[str, asyncio.Task] = {}

  # ------------------------------------------------------------------
  # The dispatcher seam
  # ------------------------------------------------------------------

  async def __call__(
      self,
      session_id: str,
      pending: list[dict],
      *,
      launch_run_id: str | None = None,
  ) -> str | None:
    """Reserve the consumer for one node and schedule its launch.

        Fresh dispatch reserves a stable Run under the control lock and claims
        the exact pending batch; an empty claim means a concurrent dispatch
        won the reservation and this call schedules nothing. A queued Run
        (an explicit retry, or a run a crashed process registered) is launched
        as its own pending execution request. Both paths run the pre-launch
        rechecks before any process starts.
        """
    tree = self._tree
    async with tree.control_lock:
      meta = await tree.load_task_meta(session_id)
      if tree.task_state(session_id) != "open":
        return None
      if launch_run_id is not None:
        run = await tree.runs.get_run(session_id, launch_run_id)
        if run is None:
          raise TaskNotFoundError(run_not_found_in_task_text(launch_run_id, session_id))
        events = tree.runs.load_events_sync(session_id)
        if (tree.runs.run_has_terminal_fact(run, events) or run.pid is not None or
            tree.runs.stop_requested(events, run.id)):
          return None  # stopped, finished, or already launching: never a second process
        if run.kind == "manager_turn" and not run.input_event_ids:
          if not pending:
            # A reservation whose claim was lost with nothing left
            # pending has no consumable input: launching it would be
            # an empty side-effecting turn. It stays queued, claims
            # nothing, and the node's decision explains it.
            log.info("queued_run_void_reservation", session_id=session_id, run_id=run.id)
            return None
          # A queued retry claims the node's pending batch when it
          # launches — the retried failure released it, and this is
          # the serialized turn that consumes it. The deterministic
          # repair also covers a run registered before a crash
          # separated it from its claim.
          await tree.dispatch.claim_input_batch_locked(session_id, run.id)
        run_id = run.id
      else:
        # The batch is re-derived inside this lock hold: the caller's
        # pending snapshot was taken without the lock, so a concurrent
        # dispatch may have consumed part of it. Reserving against the
        # stale snapshot would register a Run that can never claim
        # anything — a void queued record no terminal fact ever
        # resolves.
        pending_now = tree.dispatch.pending_inputs(session_id)
        if not pending_now:
          return None
        backend, model = self._reserve_backend_model(meta)
        request_id = "dispatch:" + sha256_hex("\x00".join(sorted(str(e.get("id")) for e in pending_now)))
        run_id = stable_run_id(session_id, request_id)
        existing = await tree.runs.get_run(session_id, run_id)
        if existing is None:
          kind = "manager_turn" if meta.profile == "manager" else "work"
          record = RunRecord(
              id=run_id,
              session_id=session_id,
              kind=kind,  # type: ignore[arg-type]
              backend=backend,
              model=model)
          # This lock hold already covers the reservation: the locked
          # registration variant, then the locked batch claim.
          await tree.runs.register_run_locked(record, task_spec_text=canonical_task_spec_text(meta.task))
        try:
          bound = await tree.dispatch.claim_input_batch_locked(session_id, run_id)
        except TaskConflictError as e:
          log.info("dispatch_reservation_contested", session_id=session_id, run_id=run_id, error=str(e))
          return None
        if not bound:  # unreachable: the batch was read under this lock hold
          raise RuntimeError(
              f"dispatch reserved run {run_id} against a batch that vanished "
              f"within one lock hold ({session_id})")
      key = (session_id, run_id)
      if key in self._launch_inflight:
        return run_id  # the concurrent winner schedules the launch
      self._launch_inflight.add(key)
      self._arm_launch_settlement(key)
    # This call owns the launch guard now; schedule directly (launch()
    # would early-return on the guard this reservation just took).
    self._schedule_launch(session_id, run_id)
    return run_id

  def _reserve_backend_model(self, meta: SessionMetadata) -> tuple[str, str | None]:
    """The task's backend+model for a fresh reservation, resolved strictly."""
    from src.core.spawner_backends import _resolve_session_default_backend_model
    return _resolve_session_default_backend_model(self._cfg, meta)

  # ------------------------------------------------------------------
  # Common launch interface
  # ------------------------------------------------------------------

  def _arm_launch_settlement(self, key: tuple[str, str]) -> None:
    """Arm the launch-settlement future one waiting controller will observe."""
    if key not in self._launch_settlements:
      self._launch_settlements[key] = asyncio.get_running_loop().create_future()

  def _settle_launch(self, key: tuple[str, str], verdict: str) -> None:
    """Resolve the launch-settlement future; drop the entry once resolved."""
    future = self._launch_settlements.pop(key, None)
    if future is not None and not future.done():
      future.set_result(verdict)

  def launch(self, session_id: str, run_id: str, *, prompt: str | None = None) -> None:
    """Schedule one registered Run's execution (the common launch seam).

        The delegate path, the review chain, and the later controller stages
        (improve, cron, triggers) all enter here; the dispatcher's executor
        seam lands on the same code through :meth:`__call__`. ``prompt`` is the
        sequence controllers' explicit launch text (an improve iteration's
        composed description, a cron step's prompt): the Run's own input batch
        stays what its registration bound, and the override is persisted onto
        the Run as the launch-text evidence before any process starts.
        """
    key = (session_id, run_id)
    if key in self._launch_inflight:
      return
    self._launch_inflight.add(key)
    self._arm_launch_settlement(key)
    self._schedule_launch(session_id, run_id, prompt=prompt)

  # ------------------------------------------------------------------
  # End-landing retry (out-of-space run endings converge without a restart)
  # ------------------------------------------------------------------

  def start_run_end_landing_retry(self, session_id: str, run_id: str | None = None) -> None:
    """Start the node's end-landing retry task; a second request is a no-op.

        One retry task per node exists at a time. The task re-runs the node's
        boot reconcile pass immediately, then every
        :data:`RUN_END_LANDING_RETRY_INTERVAL_SECONDS`, until a round completes
        without an out-of-space error — the pass drains dead runs, re-attaches
        live ones, repairs half-written end metadata, and replays the
        stable-id-deduped deliveries. No durable state: every round re-derives
        everything from on-disk facts, the task lives in process memory only,
        and the next boot's reconcile takes over after a stop.
        """
    if session_id in self._landing_retries:
      return
    from src.core.tasks import create_logged_task

    self._landing_retries[session_id] = create_logged_task(
        self._run_end_landing_retry(session_id, run_id), name=f"run-end-landing-retry-{session_id[:8]}")

  async def _run_end_landing_retry(self, session_id: str, run_id: str | None) -> None:
    """The retry loop: reconcile the node until a round raises no out-of-space.

        A round that raises out-of-space logs ``run_end_landing_retry`` and
        waits one interval. Any other exception keeps the existing node-error
        logging and ends the task — a non-space error never retries.
        """
    try:
      while True:
        try:
          from src.core import task_recovery
          counters = {"nodes": 0, "resumed": 0, "drained": 0, "followups": 0}
          await task_recovery._reconcile_node(
              session_id,
              self._tree,
              self,
              counters,
              self._cfg,
              is_driven=lambda run: self._drives_run(session_id, run))
          return  # a clean round ends the retry
        except asyncio.CancelledError:
          raise
        except Exception as exc:
          if not is_out_of_space_error(exc):
            log.exception("task_recovery_node_failed", session=session_id)
            return
          log.warning(
              "run_end_landing_retry", session=session_id, run_id=run_id, error=f"{type(exc).__name__}: {exc}"[:500])
          await asyncio.sleep(RUN_END_LANDING_RETRY_INTERVAL_SECONDS)
    finally:
      self._landing_retries.pop(session_id, None)

  def _drives_run(self, session_id: str, run: RunRecord) -> bool:
    """Whether this process is already driving *run*: its execute task is in
    flight, or a resume follow (worker or master-queue manager turn) holds it."""
    key = (session_id, run.id)
    return key in self._launch_inflight or key in self._resume_follows

  def follow_run_in_background(self, session_id: str, run_id: str) -> None:
    """Schedule one run's resume follow the way boot reconcile's step 2 does,
    handing an out-of-space follow failure to the end-landing retry entry.

        The follow pair is registered here, before the task exists: between
        this schedule and the follow's first slice another coroutine (the
        end-landing retry's node reconcile pass) can judge the same run, and
        an unregistered window there would schedule a second follower.
        """
    from src.core.tasks import create_logged_task

    self._resume_follows.add((session_id, run_id))
    create_logged_task(self._follow_run_background(session_id, run_id), name=f"task-resume-{run_id[:8]}")

  async def _follow_run_background(self, session_id: str, run_id: str) -> None:
    try:
      await self.resume_run(session_id, run_id)
    except Exception as exc:
      if is_out_of_space_error(exc):
        self.start_run_end_landing_retry(session_id, run_id)
      raise

  def _track_manager_follow(self, session_id: str, run_id: str, future: asyncio.Future) -> None:
    """Release the manager-turn follow's registry key when the master queue
    resolves it, and hand an out-of-space follow failure to the retry entry.

        The future is never awaited — boot reconcile must not wait on the
        master queue — so the done-callback is the one release point (a worker
        follow releases in resume_run's own finally).
        """

    def _follow_done(fut: asyncio.Future) -> None:
      self._resume_follows.discard((session_id, run_id))
      if fut.cancelled():
        return
      exc = fut.exception()
      if exc is not None and is_out_of_space_error(exc):
        self.start_run_end_landing_retry(session_id, run_id)

    future.add_done_callback(_follow_done)

  def _schedule_launch(
      self,
      session_id: str,
      run_id: str,
      *,
      prompt: str | None = None,
  ) -> None:
    """Fire-and-forget the actual execution; the reservation is already durable."""
    from src.core.tasks import create_logged_task

    async def _execute_and_release() -> None:
      key = (session_id, run_id)
      try:
        verdict = await self.execute_run(session_id, run_id, launch_prompt=prompt)
      except Exception as exc:
        # The launch died before the run's execution could land a
        # terminal fact: the waiting controller must see the actual
        # failure instead of polling forever behind it. The exception
        # still propagates (the logging task owner records it).
        self._settle_launch(key, f"failed-to-start: {exc}")
        if is_out_of_space_error(exc):
          await self._note_launch_out_of_space(session_id, run_id)
        raise
      finally:
        self._launch_inflight.discard(key)
      self._settle_launch(key, verdict)

    create_logged_task(_execute_and_release(), name=f"task-run-{run_id[:8]}")

  async def _note_launch_out_of_space(self, session_id: str, run_id: str) -> None:
    """Hook 1 of the end-landing retry: the launch path died out of space.

        When the run's process is dead, the worker header timer the finish
        would have closed stays open — close it here (process memory only, no
        disk write) — and start the node's retry task so the node reconcile
        pass drains the run from its raw log and lands the end record once
        space returns.
        """
    run = await self._tree.runs.get_run(session_id, run_id)
    if run is not None and run.pid is not None:
      from src.core.runs import read_host_boot_time
      if not runs.is_run_alive(run.pid, run.pid_start, run.started_at, read_host_boot_time()):
        from src.core.thinking_state import clear_run_busy
        clear_run_busy(session_id, run_id)
    self.start_run_end_landing_retry(session_id, run_id)

  async def execute_run(
      self,
      session_id: str,
      run_id: str,
      *,
      launch_prompt: str | None,
  ) -> str:
    """Pre-launch rechecks, then execute one Run on its kind's adapter.

        Rechecks run under the control lock immediately before the launch:
        role (the kind/profile pairing), open ancestry and any durable stop
        request. There is no authorization re-judgment here — the takeoff
        gate is a request-entry check (delegation, improve, agent messages to
        worker nodes), so review Runs, manual retries, startup recovery and
        cron fires are never withheld by later conversation. The backend
        resolution afterwards is explicit — a missing or invalid backend fails
        visibly, never silently substituted. ``launch_prompt`` is the sequence
        controllers' explicit launch text; see :meth:`launch`.

        Returns ``LAUNCH_STARTED`` when the Run's execution adapter was
        entered, or the actual refusal reason when a launch precondition
        withheld it (no process started and no terminal fact will arrive). A
        withheld launch records its durable ``run_launch_withheld`` event on
        the node — task closed, a durable stop request, or prompt assembly
        failed — and one blocked child report to the parent (once per run and
        reason, by stable id). An exception escaping before the adapter was
        entered is a
        failed-to-start launch: the caller's settlement sees it, never an
        endless wait.
        """
    tree = self._tree
    # The withheld verdict (closed node or durable stop) is recorded after
    # the lock releases: the durable record and the parent report take the
    # lock themselves.
    withheld: tuple[SessionMetadata, RunRecord, str] | None = None
    async with tree.control_lock:
      meta = await tree.load_task_meta(session_id)
      run = await tree.runs.get_run(session_id, run_id)
      if run is None:
        raise RunNotFoundError(run_not_found_in_task_text(run_id, session_id))
      state = tree.task_state(session_id)
      if state != "open":
        reason = f"task {session_id} is {state}"
        log.info("run_launch_withheld", session_id=session_id, run_id=run_id, reason=reason)
        withheld = (meta, run, reason)
      else:
        await tree._require_open_ancestry(session_id)
        events = tree.runs.load_events_sync(session_id)
        if tree.runs.run_has_terminal_fact(run, events):
          log.info("run_launch_refused_by_facts", session_id=session_id, run_id=run_id)
          return f"refused: run {run_id} already finished"
        if tree.runs.stop_requested(events, run_id):
          reason = f"run {run_id} has a durable stop request"
          log.info("run_launch_withheld", session_id=session_id, run_id=run_id, reason=reason)
          withheld = (meta, run, reason)
        # Trade-off 1: worker-class runs are the write-heavy launches
        # (environment installs, test runs) — below the configured free-space
        # floor they stay queued with a blocked report instead of dying
        # mid-run. Manager turns always launch: they write little and are the
        # path that reports the shortage.
        elif meta.profile == "worker" and run.kind in WORKER_KINDS:
          reason = self._disk_headroom_withhold_reason()
          if reason is not None:
            log.info("run_launch_withheld", session_id=session_id, run_id=run_id, reason=reason)
            withheld = (meta, run, reason)
    if withheld is not None:
      meta, run, reason = withheld
      return await self._record_launch_withheld(meta, run, reason)
    # Backend resolution is the first post-admission act: an exception here
    # is a launch failure on an ADMITTED run and lands the run's durable
    # failure (terminal fact, error evidence, after-run) before it
    # propagates to the logging task owner.
    try:
      option = self._resolve_run_backend(run)
    except Exception as exc:
      await self._land_launch_failure(meta, run, exc)
      raise
    # The one assembly owner builds this launch's managed instructions and
    # their durable snapshot BEFORE any backend is invoked. A preparation
    # failure (missing/corrupt rule, malformed memory, unsettled sources)
    # is a definitely-unlaunched withheld verdict: the queued Run and its
    # unconsumed inputs stay exactly as they were (the launch settles
    # failed-to-start; no terminal fact — pinned by
    # test_snapshot_publish_failure_is_a_definitely_unlaunched_preparation_failure).
    try:
      snapshot = await self._prepare_launch_snapshot(meta, run, option)
    except TaskPromptError as e:
      log.error("task_prompt_preparation_failed", session_id=session_id, run_id=run_id, kind=run.kind, error=str(e))
      return await self._record_launch_withheld(meta, run, f"prompt preparation failed: {e}")
    # Adapter entry to process start: context build, worktree preparation
    # and spawn all happen inside. An exception there is a launch failure
    # on an admitted run — the same durable-failure handler as backend
    # resolution.
    try:
      if meta.profile == "manager" and run.kind == "manager_turn":
        await self._execute_manager_turn(meta, run, option, snapshot)
      elif meta.profile == "worker" and run.kind in WORKER_KINDS:
        await self._execute_worker_run(meta, run, option, snapshot, launch_prompt=launch_prompt)
      else:
        raise TaskInvalidError(f"run {run_id} (profile={meta.profile}, kind={run.kind}) has no executable adapter")
    except Exception as exc:
      await self._land_launch_failure(meta, run, exc)
      raise
    return LAUNCH_STARTED

  def _disk_headroom_withhold_reason(self) -> str | None:
    """The worker-class precheck's disk verdict, or None when launching may proceed.

        Both filesystems a worker launch writes to are checked: the one holding
        the CharlieBot data dir and the one holding the worktree root.
        ``server.min_free_disk_gib`` 0 disables the check.
        """
    min_gib = self._cfg.server.min_free_disk_gib
    if min_gib <= 0:
      return None
    for path in (self._cfg.charliebot_home, Path(self._cfg.paths.worktree_dir)):
      free_gib = free_disk_gib(path)
      if free_gib < min_gib:
        return f"disk free {free_gib:.1f} GiB, below {min_gib} GiB ({path})"
    return None

  async def _record_launch_withheld(self, meta: SessionMetadata, run: RunRecord, reason: str) -> str:
    """Record one withheld launch durably and report it to the parent.

        The node's events log gains one ``run_launch_withheld`` fact per
        (run, reason) pair — the stable id dedups repeated launch attempts and
        recovery passes, and ``run_display_state`` reads it as the queued Run's
        ``withheld`` display state with its reason. One ``blocked``
        child_report sourced from that event persists for the parent and wakes
        it through ``wake_parent``; the stable report id keeps it to one
        report per (run, reason) as well. Returns the launch verdict string.
        """
    tree = self._tree
    session_id, run_id = meta.id, run.id
    event_id = stable_withheld_event_id(run_id, reason)
    epoch = await tree.sessions.prime_aggregator(session_id)
    parent_epoch = (await tree.sessions.prime_aggregator(meta.task_parent_id) if meta.task_parent_id else None)
    async with tree.control_lock:
      events = tree.fact_history(session_id)
      existing = next((e for e in events if e.get("id") == event_id), None)
      if existing is not None:
        event = existing
      else:
        event = build_control_event(
            ET.RUN_LAUNCH_WITHHELD,
            actor=ACTOR_SYSTEM,
            source_session_id=session_id,
            event_id=event_id,
            run_id=run_id,
            reason=reason,
        )
        await tree.events.append(session_id, event)
      report_event = None
      report_created = False
      if meta.task_parent_id:
        report_event, report_created = await tree.dispatch.deliver_child_report_locked(
            session_id,
            source_event=event,
            outcome="blocked",
            summary=f"launch withheld: {reason}",
            result_refs=[f"run:{run_id}"],
            recipient=meta.task_parent_id,
            actor=ACTOR_SYSTEM,
        )
    await tree.sessions.announce_appended_event(session_id, event, epoch=epoch)
    if report_event is not None and report_created and parent_epoch is not None:
      await tree.sessions.announce_appended_event(meta.task_parent_id, report_event, epoch=parent_epoch)
    if report_event is not None and report_created and meta.task_parent_id:
      await tree.dispatch.wake_parent(meta.task_parent_id, report=report_event)
    return f"withheld: {reason}"

  async def _land_launch_failure(self, meta: SessionMetadata, run: RunRecord, exc: Exception) -> None:
    """Land one post-admission launch exception as the Run's durable failure.

        The admitted run's evidence trail: the error text rides the run's
        events log (where the leaf card's event view and the failure-summary
        reader already look), the terminal ``run_finished`` fact carries
        outcome ``failed``, and the failed run then walks the same after-run
        path a failed process takes — the worker's failure report to its
        parent (task-tree or legacy) and the node's pending-input dispatch.
        Withheld and refused launch verdicts never reach this handler: they
        keep their no-terminal-fact semantics. The exception itself still
        propagates to the launch task's logging owner with its traceback.

        A launched run (it has a pid) that failed out of space is the one
        exception to the immediate landing: only the error evidence is
        written, the run stays unfinished, and the node reconcile pass drains
        it from its raw log once space returns — landing a failed end record
        here could contradict a success fact the delivery stage has yet to
        read. Any other error keeps today's immediate landing, whose delivery
        reads the durable outcome (never a literal ``failed``): a run with a
        success fact that hits a further write error during delivery must not
        send a contradicting failed report.
        """
    session_id, run_id = meta.id, run.id
    log.error(
        "task_run_launch_failed", session_id=session_id, run_id=run_id, kind=run.kind, error=str(exc), exc_info=True)
    # The precheck-time record predates the launch: the pid (and worktree
    # facts) landed during execution, so the durable record decides whether
    # this run launched.
    fresh = await self._tree.runs.get_run(session_id, run_id)
    if fresh is not None:
      run = fresh
    if run.pid is not None and is_out_of_space_error(exc):
      try:
        error_text = f"{type(exc).__name__}: {exc}"[:2000]
        await self._record_launch_error_event(session_id, run_id, error_text)
      except Exception as land_exc:
        log.error(
            "task_run_launch_failure_landing_failed",
            session_id=session_id,
            run_id=run_id,
            error=str(land_exc),
            exc_info=True)
      return
    try:
      error_text = f"{type(exc).__name__}: {exc}"[:2000]
      await self._record_launch_error_event(session_id, run_id, error_text)
      await self._tree.dispatch.finish_run(session_id, run_id, outcome="failed", exit_code=-1)
      fresh = await self._tree.runs.get_run(session_id, run_id)
      if fresh is not None:
        run = fresh
      if meta.profile == "worker":
        # The durable outcome, exactly as _finalize_worker_run reads it: the
        # first terminal fact wins over this landing's literal "failed".
        durable = await self._tree.runs.terminal_outcome_of(session_id, run_id) or "failed"
        await self._after_worker_run(meta, run, durable)
      else:
        await self._tree.dispatch.dispatch_pending(session_id)
    except Exception as land_exc:
      # A failed landing must never mask the launch failure that caused
      # it: both stay in the log, and the original still propagates.
      log.error(
          "task_run_launch_failure_landing_failed",
          session_id=session_id,
          run_id=run_id,
          error=str(land_exc),
          exc_info=True)
      if is_out_of_space_error(land_exc):
        # A run that never launched keeps its queued registration: the node
        # reconcile pass's dispatch re-dispatches it after space returns, the
        # same as after a restart today.
        self.start_run_end_landing_retry(session_id, run_id)

  async def _record_launch_error_event(self, session_id: str, run_id: str, error_text: str) -> None:
    """Write the launch error into the run's events log as durable evidence.

        The events log is where the leaf card's event view already reads and
        where ``_worker_failure_summary`` looks for the run's closing words, so
        the error is reachable without any new evidence channel.
        """
    from src.core.ndjson import append_ndjson

    events_log = self._tree.runs.run_dir(session_id, run_id) / RUN_EVENTS_NAME
    await append_ndjson(
        events_log, {
            "type": ET.ERROR,
            "message": error_text,
            "content": error_text,
            "timestamp": utc_now_iso(),
        })
    # The leaf card's Events link reads the record's events_ref: the
    # launch-failed run's evidence is reachable exactly the way a process
    # run's is (record_observation writes only the provided fields).
    await self._tree.runs.record_observation(session_id, run_id, events_ref=str(events_log))

  async def _await_terminal(self, session_id: str, run_id: str) -> str:
    """Await one Run's durable terminal fact; returns its outcome."""
    tree = self._tree
    while True:
      outcome = await tree.runs.terminal_outcome_of(session_id, run_id)
      if outcome is not None:
        return outcome
      await asyncio.sleep(0.2)

  async def launch_and_settle(
      self,
      session_id: str,
      run_id: str,
      *,
      prompt: str | None = None,
  ) -> LaunchSettlement:
    """The sequence controllers' shared launch/wait observation.

        Launches one registered Run (or joins the in-flight launch) and
        settles to either its durable terminal outcome or an explicit
        withheld verdict. Lifecycle facts decide which: a Run that already
        carries a terminal fact returns it; a live or ended process is
        followed to its fact (never relaunched, never killed for running
        long); only a launch whose precondition failed in the
        registration-to-launch interval — node closed, durable stop, prompt
        assembly failure — settles withheld, with the actual reason. The
        first terminal fact always wins over a settle race.
        """
    tree = self._tree
    run = await tree.runs.get_run(session_id, run_id)
    if run is None:
      raise RunNotFoundError(run_not_found_in_task_text(run_id, session_id))
    outcome = await tree.runs.terminal_outcome_of(session_id, run_id)
    if outcome is not None:
      return LaunchSettlement(outcome=outcome)
    if run.pid is not None:
      # A process this launch must never duplicate (a replayed
      # registration, or a follow that outlives the controller): follow
      # it to its durable fact.
      return LaunchSettlement(outcome=await self._await_terminal(session_id, run_id))
    key = (session_id, run_id)
    if key not in self._launch_settlements:
      self.launch(session_id, run_id, prompt=prompt)
    future = self._launch_settlements.get(key)
    if future is None:
      # The launch settled (and dropped its entry) between the facts
      # read above and now: re-read what actually happened.
      outcome = await tree.runs.terminal_outcome_of(session_id, run_id)
      if outcome is not None:
        return LaunchSettlement(outcome=outcome)
      raise RuntimeError(f"run {run_id} settled without a terminal fact or launch verdict")
    verdict = await future
    if verdict != LAUNCH_STARTED:
      # First terminal fact wins over a settle race: the run may have
      # finished through another path while this verdict was formed.
      outcome = await tree.runs.terminal_outcome_of(session_id, run_id)
      if outcome is not None:
        return LaunchSettlement(outcome=outcome)
      return LaunchSettlement(withheld=verdict)
    return LaunchSettlement(outcome=await self._await_terminal(session_id, run_id))

  def _resolve_run_backend(self, run: RunRecord) -> BackendOption:
    """The Run's explicitly recorded backend/model, resolved strictly."""
    if not run.backend:
      raise ValueError(f"run {run.id} records no backend; refusing to substitute one")
    return resolve_backend_option(self._cfg, run.backend, run.model)

  def _child_env(self, session_id: str, run_id: str, agent_name: str) -> dict[str, str]:
    """The child CLI environment: its own identity, home and address.

        The child's session id and its own signed run token travel explicitly —
        never the parent's inherited identity — and CHARLIEBOT_HOME pins the
        selected home so the child CLI resolves this instance's config,
        credentials and server address.
        """
    key = configured_access_key()
    if not key:
      raise RuntimeError(
          "run-token signing requires credentials.yaml charliebot.access_key; "
          "agent child environments cannot be built without it")
    token = sign_run_token(RunTokenClaims(session_id=session_id, run_id=run_id, agent=agent_name or "worker"), key)
    return {
        SESSION_ID_ENV_VAR: session_id,
        RUN_TOKEN_ENV: token,
        "CHARLIEBOT_HOME": str(self._cfg.charliebot_home),
    }

  # ------------------------------------------------------------------
  # Manager turns
  # ------------------------------------------------------------------

  async def _alert_overlay_inactive(
      self,
      session_id: str,
      option: BackendOption,
      overlay_error: OSError | None,
  ) -> None:
    """The unified fenceless-run alert (undeclared or unreadable overlay)."""
    reason = "unreadable" if overlay_error is not None else "undeclared"
    log.warning("task_overlay_inactive", session_id=session_id, backend=option.id, reason=reason)
    await self._sessions.callbacks().persist_and_broadcast(
        session_id, {
            "type":
                ET.BACKEND_OVERLAY_INACTIVE,
            "backend":
                option.id,
            "reason":
                reason,
            **(
                {
                    "overlay": option.prompt_overlay,
                    "error": type(overlay_error).__name__
                } if overlay_error is not None else {}),
        })

  async def _prepare_launch_snapshot(
      self,
      meta: SessionMetadata,
      run: RunRecord,
      option: BackendOption,
  ) -> PromptSnapshot:
    """Build, recheck, and durably commit this launch's instruction snapshot.

        The committed bytes are the bytes the adapters launch — never a second
        build. A declared-but-unreadable or undeclared overlay emits the unified
        fenceless-run alert exactly like the v1 wake path.
        """
    snapshot, overlay_error, declared = await assemble_coherent_snapshot(self._cfg, self._tree, meta, run.kind, option)
    path = self._tree.runs.run_dir(meta.id, run.id) / task_prompts.SNAPSHOT_FILENAME
    from src.core.json_utils import atomic_write_text
    await asyncio.to_thread(atomic_write_text, path, json.dumps(snapshot.to_json_dict(), indent=2, ensure_ascii=False))
    await self._tree.runs.record_observation(meta.id, run.id, prompt_snapshot_ref=str(path))
    if not declared or overlay_error is not None:
      await self._alert_overlay_inactive(meta.id, option, overlay_error)
    return snapshot

  def _manager_finish_recorder(
      self, session_id: str, run_id: str, option: BackendOption, transport_dir: Path, *,
      ended_at: datetime | None) -> Callable[[str | None, int, dict], Awaitable[None]]:
    """The finish recorder both manager-turn launch paths hand the master queue.

    Records the run observation (native session id, served model, raw-log refs),
    lands the terminal fact through ``dispatch.finish_run``, and re-drives the
    node's pending inputs. ``ended_at=None`` keeps record_finish's
    observed-write-time default; the resume follow passes the drain rule's end
    time.
    """

    async def on_task_finish(cc_session_id: str | None, exit_code: int, finish_extras: dict) -> None:
      await self._tree.runs.record_observation(
          session_id,
          run_id,
          native_session_id=cc_session_id,
          model=finish_extras.get("model") or option.model,
          raw_log_ref=str(transport_dir / runs.RAW_LOG_NAME),
          result_ref=str(transport_dir / runs.RAW_LOG_NAME),
      )
      await self._tree.dispatch.finish_run(
          session_id,
          run_id,
          outcome="success" if exit_code == 0 else "failed",
          exit_code=exit_code,
          ended_at=ended_at,
      )
      # Inputs admitted during this turn waited for the serialized
      # consumer; the turn's finish is what dispatches their next run.
      await self._tree.dispatch.dispatch_pending(session_id)

    return on_task_finish

  async def _execute_manager_turn(
      self,
      meta: SessionMetadata,
      run: RunRecord,
      option: BackendOption,
      snapshot: PromptSnapshot,
  ) -> None:
    """One manager turn on the existing per-session master queue.

        The turn's input is the exact durable batch the Run claimed; the
        existing queue serializes turns per node, streams events into the
        session chat, and keeps the native continuation anchor on the stable
        session. The launch's managed instructions are the committed snapshot's
        bytes — delivered through the backend's system-instruction seam, never
        rebuilt here. The Run is the sole new execution record: pid/pid_start
        land at spawn, native_session_id/model/raw log land at finish, and the
        terminal fact goes through ``dispatch.finish_run``.

        Native continuation rule: the anchor's conversation continues only when
        the effective instruction hash AND the backend identity are unchanged;
        a rules/source change starts a fresh native context carrying a reset
        notice (the task summary and where the earlier history lives), while an
        input-only change keeps the conversation.
        """
    from src.agents.master_cc import run_message
    from src.agents.master_cc_state import TaskRunBinding

    session_id, run_id = meta.id, run.id
    transport_dir = self._tree.runs.run_dir(session_id, run_id)
    batch_ids = set(run.input_event_ids)
    batch_events = [e for e in self._tree.fact_history(session_id) if str(e.get("id")) in batch_ids]
    content, uploaded_files = compose_input_prompt(batch_events)
    if not content:
      raise TaskInvalidError(f"run {run_id} claimed no consumable input; nothing to execute")

    # The dispatched wake of a task-bound node takes over the cron
    # session's duties (weekly recycle, firing-report prefix) here — the
    # legacy trigger_master wake never reaches a task-tree node. The
    # recycle may clear the node's anchor in place, so the
    # fresh-conversation judgment below reads the post-recycle state.
    from src.core.master_trigger import apply_bound_wake_duties
    scheduled_prefix = await apply_bound_wake_duties(self._sessions, meta, batch_events)
    if scheduled_prefix:
      content = f"{scheduled_prefix}{content}"

    anchor_continues = (
        meta.cc_session_id is not None and meta.native_prompt_hash == snapshot.prompt_hash and
        meta.native_backend == option.id and meta.native_model == option.model)
    fresh_native = not anchor_continues
    prompt = content
    if fresh_native and meta.cc_session_id is not None:
      # A reset, not a first turn: name the task and where the earlier
      # history lives, so the fresh native context can catch up without
      # the old conversation being copied or destroyed. A backend change
      # names the switch; every other change keeps the standing reason.
      if meta.native_backend and meta.native_backend != option.id:
        reason = backend_switch_reset_reason(meta.native_backend, option.id)
      else:
        reason = (
            "this task's managed instructions or sources changed since the "
            "previous turn, so this turn starts a fresh native conversation")
      goal = meta.task.goal if meta.task is not None else meta.name
      prompt = f"{context_reset_note(reason, goal)}\n\n{content}"

    async def on_task_spawn(pid: int, pid_start: str | None) -> None:
      if pid_start is None:
        raise RuntimeError(f"run {run_id} spawned without a pinned pid_start")
      await self._tree.runs.record_launch(session_id, run_id, pid=pid, pid_start=pid_start)
      # The anchor decision is durable the moment the process exists: a
      # reset clears the anchor here (never during preparation, which may
      # fail without touching the usable old anchor), and the identity
      # fields pin the snapshot this conversation continues under.
      await self._tree.record_native_anchor(
          session_id,
          prompt_hash=snapshot.prompt_hash,
          backend=option.id,
          model=option.model,
          reset_anchor=fresh_native)

    on_task_finish = self._manager_finish_recorder(session_id, run_id, option, transport_dir, ended_at=None)

    await self._persist_launch_text(session_id, run_id, prompt)
    log.info(
        "manager_turn_launching",
        session_id=session_id,
        run_id=run_id,
        backend=option.id,
        inputs=len(run.input_event_ids),
        prompt_hash=snapshot.prompt_hash[:12],
        fresh_native=fresh_native)
    await run_message(
        self._cfg,
        meta,
        prompt,
        self._sessions.callbacks(),
        # Not a legacy input delivery: the Run's item carries a task_run
        # binding and never takes part in a batch.
        input_event_type=None,
        skip_user_event=True,
        auto_trigger=any(e.get("type") == ET.SCHEDULED_TRIGGER for e in batch_events),
        backend_option=option,
        uploaded_files=uploaded_files or None,
        expect_fresh_session=fresh_native,
        task_instructions=snapshot.instructions_text,
        task_run=TaskRunBinding(
            session_id=session_id, run_id=run_id, transport_dir=str(transport_dir), fresh_native_context=fresh_native),
        on_task_spawn=on_task_spawn,
        on_task_finish=on_task_finish,
        extra_env=self._child_env(session_id, run_id, meta.name),
    )

  # ------------------------------------------------------------------
  # Worker work and review runs
  # ------------------------------------------------------------------

  async def _execute_worker_run(
      self,
      meta: SessionMetadata,
      run: RunRecord,
      option: BackendOption,
      snapshot: PromptSnapshot,
      *,
      launch_prompt: str | None,
  ) -> None:
    """One work, review, or sequence Run on the existing Worker/backend adapter.

        The context owner supplies every applicable managed rule/memory block
        exactly once — the committed ``snapshot`` rides the backend's
        system-instruction seam — while the controllers keep owning their task /
        step input: this adapter renders only the task/input context (bindings,
        pinned spec, claimed batch, sequence positions) from the same maintained
        template sections. A scheduled prompt override no longer bypasses the
        assembly.
        """
    session_id, run_id = meta.id, run.id
    run_dir = self._tree.runs.run_dir(session_id, run_id)
    events_log = run_dir / RUN_EVENTS_NAME

    task_type = task_prompts.prompt_task_type(meta.task)
    review_worktree: str | None = None
    if run.kind == "review":
      work_run = await self._tree.runs.get_run(session_id, run.review_of_run_id or "")
      if work_run is None:
        raise TaskInvalidError(f"review run {run_id} names no recorded work Run")
      # The review reuses the work Run's exact repo, branch and worktree.
      review_worktree = work_run.worktree_path
      context = await self._build_review_context(session_id, work_run)
    elif run.kind == "iteration":
      context = await self._build_iteration_context(meta, run, launch_prompt)
    elif launch_prompt is not None:
      # The sequence controllers' explicit launch text (a cron step's
      # prompt rides verbatim, exactly as the legacy scheduled worker's
      # prompt_override did). The controller owns the composition; the
      # adapter renders it as the task/input context of the assembled
      # instructions.
      context = await self._build_step_context(meta, run, launch_prompt, task_type)
    else:
      context = await self._build_work_context(meta, run, task_type)
    prompt = context
    await self._persist_launch_text(session_id, run_id, prompt)

    binding = RunWorkerBinding(id=run_id, session_id=session_id)
    if option.type == BackendType.CC_CLAUDE:
      binding.claude_session_id = str(uuid.uuid4())

    async def on_spawned(spawned: RunWorkerBinding) -> None:
      if spawned.pid is None or spawned.pid_start is None:
        raise RuntimeError(f"run {run_id} spawned without a pinned process identity")
      await self._tree.runs.record_launch(session_id, run_id, pid=spawned.pid, pid_start=spawned.pid_start)

    error = ""
    exit_code = -1
    worker: Worker | None = None
    try:
      working_dir = Path(review_worktree) if review_worktree else run_dir
      # A pooled cc-claude backend launches on the pool account with the most
      # headroom, so the Worker's relay loop (src/core/claude_relay.py) can
      # move the run when that login is rejected mid-run. The selection sits
      # inside this try: with no account available the run fails before any
      # process starts, through the pool-exhausted branch below.
      claude_account: ClaudeAccount | None = None
      if claude_accounts.is_pooled(option, self._cfg):
        claude_account = claude_accounts.select(self._cfg, option.model)
        if claude_account is None:
          raise claude_relay.PoolExhaustedError(claude_relay.pool_exhausted_message(self._cfg))
      worker = Worker(
          binding,  # type: ignore[arg-type]
          working_dir,
          events_log,
          prompt,
          self._cfg,
          backend_option=option,
          claude_account=claude_account,
          on_spawned=on_spawned,
          extra_env=self._child_env(session_id, run_id, meta.name),
          instructions_content=snapshot.instructions_text,
      )
      # Session-level notices (a pool login that needs re-login) reach
      # the session chat through the successor chain, exactly as the
      # legacy worker path delivers them.
      worker.on_session_event = functools.partial(self._sessions.deliver_to_successor, session_id)
      exit_code = await worker.run()
    except claude_relay.PoolExhaustedError as exc:
      if worker is not None:
        await worker.terminate()
      error = str(exc)
      log.warning("task_run_pool_exhausted", session_id=session_id, run_id=run_id, error=error)
      # The run's own events log carries the evidence: the failure-summary
      # reader and the improve quota classification read this event.
      await self._record_launch_error_event(session_id, run_id, error)
    except QuotaExhaustedError as exc:
      if worker is not None:
        await worker.terminate()
      error = str(exc)
      log.warning("task_run_quota_exhausted", session_id=session_id, run_id=run_id, error=error)
    except Exception as exc:  # setup/transport failure: the run failed loudly
      log.error("task_run_failed", session_id=session_id, run_id=run_id, error=str(exc), exc_info=True)
      if worker is not None:
        await worker.terminate()
      error = str(exc)

    durable_outcome = await self._finalize_worker_run(meta, run, option, exit_code=exit_code, error=error)
    # The launch's own writes (worktree facts, native session id) landed on
    # the DURABLE record after this in-memory snapshot was taken; the
    # delivery chain must judge the recorded provenance, not the stale one.
    fresh = await self._tree.runs.get_run(session_id, run_id)
    if fresh is not None:
      run = fresh
    await self._after_worker_run(meta, run, durable_outcome)

  async def _finalize_worker_run(
      self,
      meta: SessionMetadata,
      run: RunRecord,
      option: BackendOption,
      *,
      exit_code: int,
      error: str,
      ended_at: datetime | None = None,
  ) -> str:
    """Land one worker Run's observation and terminal fact; returns the durable outcome.

        Successful process exit is only one input: the durable outcome requires
        a successful result event in the run's raw transport log — empty
        output or a missing result lands the failed outcome with the evidence
        retained. The first terminal fact wins: a stop request that
        observed the exit first stands, and this finish reconciles against it.
        """
    session_id, run_id = meta.id, run.id
    run_dir = self._tree.runs.run_dir(session_id, run_id)
    raw_path = run_dir / runs.RAW_LOG_NAME
    native_session_id = await self._native_session_id(raw_path, option)
    outcome = await self._worker_outcome(run_dir, option)
    await self._tree.runs.record_observation(
        session_id,
        run_id,
        native_session_id=native_session_id,
        model=option.model,
        raw_log_ref=str(raw_path),
        events_ref=str(run_dir / RUN_EVENTS_NAME),
        result_ref=str(raw_path),
    )
    await self._tree.dispatch.finish_run(
        session_id, run_id, outcome=outcome, exit_code=exit_code if not error else -1, ended_at=ended_at)
    durable = await self._tree.runs.terminal_outcome_of(session_id, run_id) or outcome
    if error:
      log.warning("task_run_error", session_id=session_id, run_id=run_id, error=error[:500])
    return durable

  async def _worker_outcome(self, run_dir: Path, option: BackendOption) -> str:
    """The terminal-status judgment from the Run's own transport records.

        The raw stream is the primary record; the run's translated events log
        (the same stream's durable projection) carries the equivalent RESULT
        event and serves when a backend wrote no raw file. Successful process
        exit alone never decides: no result event anywhere is a failure with
        the evidence retained.
        """
    raw_path = run_dir / runs.RAW_LOG_NAME
    if raw_path.is_file():
      translate = self._fresh_translate(option)
      _events, result, _code = await asyncio.to_thread(scan_result_exit, raw_path, translate)
      if result is not None and runs.result_success(result):
        return "success"
    events_log = run_dir / RUN_EVENTS_NAME
    if events_log.is_file():
      _found, success = await asyncio.to_thread(self._events_log_result_success, events_log)
      if success:
        return "success"
    return "failed"

  @staticmethod
  def _events_log_result_success(events_log: Path) -> tuple[bool, bool]:
    """(found, success) of the last RESULT event in a translated events log."""
    found = False
    success = False
    for event in runs.parse_raw_lines(events_log.read_bytes()):
      if event.get("type") == ET.RESULT:
        found = True
        success = runs.result_success(event)
    return found, success

  async def _native_session_id(self, raw_path: Path, option: BackendOption) -> str | None:
    """The backend's native session id from the run's raw stream, when one exists."""
    if not raw_path.is_file():
      return None
    translate = self._fresh_translate(option)

    def _scan() -> str | None:
      for event in runs.parse_raw_lines(raw_path.read_bytes()):
        for translated in translate(event):
          if translated.get("type") == ET.SESSION_ATTACHED and translated.get("session_id"):
            return str(translated["session_id"])
      return None

    return await asyncio.to_thread(_scan)

  def _fresh_translate(self, option: BackendOption) -> Callable[[dict], list[dict]]:
    """A fresh translate callable for one whole-file scan (stateful translates need one instance)."""
    from src.agents.backends.registry import build_backend
    try:
      return build_backend(option, self._cfg).translate_event
    except Exception as e:
      # Translate-only construction may lack the CLI binary; the scan
      # degrades to the raw claude shape the same way restart recovery's
      # translate fallback does — never a crash of the finalize path.
      log.warning("task_run_translate_unresolved", backend=option.id, error=str(e))
      return lambda event: [event]

  # ------------------------------------------------------------------
  # Prompts (existing sources; the launch text rides the Run as evidence)
  # ------------------------------------------------------------------

  async def _work_description(self, meta: SessionMetadata, run: RunRecord) -> str:
    """The task/input body: the pinned spec's goal plus this run's claimed batch."""
    task = meta.task
    spec_text = task.goal if (task is not None and task.goal.strip()) else ""
    batch_ids = set(run.input_event_ids)
    batch = [e for e in self._tree.fact_history(meta.id) if str(e.get("id")) in batch_ids]
    content, _uploads = compose_input_prompt(batch) if batch else ("", [])
    description = "\n\n".join(part for part in (spec_text, content) if part)
    if not description.strip():
      raise TaskInvalidError(f"run {run.id} has no task spec and no input; nothing to execute")
    return description

  def _binding_intro(self, run: RunRecord) -> str:
    """The workflow bindings' intro line: a retry continuing the worktree says so."""
    if run.retry_of_run_id is not None and run.worktree_path is not None:
      from src.core.spawner_prompt import load_worker_prompt_sections
      return load_worker_prompt_sections(self._cfg)["intro_continuation"].strip()
    from src.core.spawner_prompt import load_worker_prompt_sections
    return load_worker_prompt_sections(self._cfg)["intro_new"].strip()

  async def _build_work_context(self, meta: SessionMetadata, run: RunRecord, task_type: TaskType) -> str:
    """The work Run's task/input context from the maintained template sections.

        Verify runs stay read-only: their contract is entirely managed
        instructions, so their context is the pinned spec and batch alone (no
        worktree, no review artifacts). Repo-less work runs keep the run dir as
        the working directory.
        """
    description = await self._work_description(meta, run)
    task = meta.task
    if task_type == TaskType.VERIFY:
      return task_prompts.render_task_body(self._cfg, description)
    parts = [task_prompts.render_session_info(self._cfg, meta.name)]
    if not (task is not None and task.repo_path):
      parts.append(task_prompts.render_task_body(self._cfg, description))
      return "\n\n".join(part for part in parts if part)
    assert task.repo_path is not None
    repo_path = Path(task.repo_path).resolve()
    base_branch = run.base_branch or task.base_branch
    branch_name = run.branch_name
    worktree_path = run.worktree_path
    start_point: str | None = None
    if not (base_branch and branch_name and worktree_path):
      base_branch, branch_name, worktree_path, start_point = await self._prepare_worktree(meta, run, repo_path)
    assert base_branch and branch_name and worktree_path
    origin = f"`{base_branch}`" + (f" @ `{start_point}`" if start_point else "")
    parts.append(
        task_prompts.render_worktree_bindings(
            self._cfg,
            task_type=task_type,
            intro_line=self._binding_intro(run),
            branch_name=branch_name,
            base_branch_origin=origin,
            wt_path=worktree_path,
            repo_path=str(repo_path)))
    parts.append(task_prompts.render_task_body(self._cfg, description))
    if task is not None and task.keep_worktree:
      parts.append(task_prompts.render_worktree_persistence(self._cfg))
    return "\n\n".join(part for part in parts if part)

  async def _build_iteration_context(self, meta: SessionMetadata, run: RunRecord, description: str) -> str:
    """One improve iteration's context: shared-worktree bindings + the loop position.

        The controller composes the description (live goal, optional plan,
        previous summaries) and passes it as the launch text; the sequence_ref
        pins the shared worktree facts — an iteration Run without them is a
        controller bug and fails loudly instead of creating a worktree of its
        own. The report contract renders from the maintained iteration section
        with the actual loop directory and position.
        """
    seq = run.sequence_ref
    if seq is None or seq.kind != "improve":
      raise TaskInvalidError(
          f"iteration run {run.id} carries no improve sequence_ref; the controller "
          "that registered it is broken")
    if not (run.repo_path and run.base_branch and run.branch_name and run.worktree_path):
      raise TaskInvalidError(
          f"iteration run {run.id} is missing its pinned shared-worktree provenance "
          "(repo/base/branch/worktree); refusing to create a divergent worktree")
    parts = [task_prompts.render_session_info(self._cfg, meta.name)]
    parts.append(
        task_prompts.render_worktree_bindings(
            self._cfg,
            task_type=TaskType.IMPLEMENT,
            intro_line=self._binding_intro(run),
            branch_name=run.branch_name,
            base_branch_origin=f"`{run.base_branch}`",
            wt_path=run.worktree_path,
            repo_path=run.repo_path))
    parts.append(task_prompts.render_task_body(self._cfg, description))
    parts.append(
        task_prompts.render_iteration_reports(self._cfg, loop_dir=seq.owner_ref, iteration_number=seq.position))
    return "\n\n".join(part for part in parts if part)

  async def _build_step_context(
      self,
      meta: SessionMetadata,
      run: RunRecord,
      step_prompt: str,
      task_type: TaskType,
  ) -> str:
    """One scheduled step's context: the controller's prompt over the task's bindings.

        The step Run shares the leaf task's worktree provenance (pinned at
        registration); the controller's prompt is the task/input body.
        """
    parts = [task_prompts.render_session_info(self._cfg, meta.name)]
    if run.worktree_path and run.branch_name and run.base_branch and run.repo_path:
      parts.append(
          task_prompts.render_worktree_bindings(
              self._cfg,
              task_type=task_type,
              intro_line=self._binding_intro(run),
              branch_name=run.branch_name,
              base_branch_origin=f"`{run.base_branch}`",
              wt_path=run.worktree_path,
              repo_path=run.repo_path))
    parts.append(task_prompts.render_task_body(self._cfg, step_prompt))
    return "\n\n".join(part for part in parts if part)

  async def _build_review_context(self, session_id: str, work_run: RunRecord) -> str:
    """The review Run's task/input context: the work being judged and its git steps.

        The reviewer's stable contract rides the managed instructions
        (review_rules_text); this context names the exact work Run, its logs,
        and the volatile git steps — it never turns the reviewer into an
        implementer beyond the checklist's minimal-fix rule. A repo-less work
        Run reviews paths instead of a diff.
        """
    if not work_run.repo_path:
      return await self._build_repo_less_review_context(session_id, work_run)
    assert (work_run.branch_name and work_run.worktree_path and work_run.repo_path and work_run.base_branch), (
        f"review of work run {work_run.id} needs its exact repo/base/branch/worktree "
        "provenance; an unset base is never silently replaced with main")
    user_request, worker_summary = await review.extract_review_context(
        session_id,
        work_run.id,
        self._cfg.sessions_dir,
        worker_log_path=self._tree.runs.run_dir(session_id, work_run.id) / RUN_EVENTS_NAME)
    context_lines = review.review_context_lines(user_request, worker_summary, f"(work run {work_run.id})")
    return task_prompts.review_task_context(
        branch_name=work_run.branch_name,
        wt_path=work_run.worktree_path,
        base_branch=work_run.base_branch,
        chat_log_path=chat_events_path(self._cfg.sessions_dir / session_id),
        worker_log_path=self._tree.runs.run_dir(session_id, work_run.id) / RUN_EVENTS_NAME,
        context_section="\n".join(context_lines),
    )

  async def _build_repo_less_review_context(self, session_id: str, work_run: RunRecord) -> str:
    """The repo-less review's task/input context: spec and reported paths, no git.

        Without a repository there is no diff to read and nothing to merge:
        the review context is the task spec (its acceptance tests included)
        plus the work report's path list, and the reviewer checks the current
        state of those paths against the acceptance tests. The spec comes from
        the task record — the same body the work Run executed against.
        """
    meta = await self._tree.load_meta(session_id)
    task = meta.task if meta is not None else None
    spec_parts = []
    if task is not None and task.goal.strip():
      spec_parts.append(task.goal)
    if task is not None and task.acceptance:
      spec_parts.append("\n".join(["## Acceptance Tests", *[f"- {a}" for a in task.acceptance]]))
    _user_request, worker_summary = await review.extract_review_context(
        session_id,
        work_run.id,
        self._cfg.sessions_dir,
        worker_log_path=self._tree.runs.run_dir(session_id, work_run.id) / RUN_EVENTS_NAME)
    return task_prompts.repo_less_review_task_context(
        chat_log_path=chat_events_path(self._cfg.sessions_dir / session_id),
        worker_log_path=self._tree.runs.run_dir(session_id, work_run.id) / RUN_EVENTS_NAME,
        spec_text="\n\n".join(spec_parts) or None,
        work_report=worker_summary,
    )

  async def _persist_launch_text(self, session_id: str, run_id: str, prompt: str) -> None:
    """Retain the launch's exact task/input text on the Run as separate evidence.

        The managed instruction half lives in the Run's committed snapshot
        (``prompt_snapshot.json`` via ``prompt_snapshot_ref``); this file keeps
        the volatile half — bindings, pinned spec, claimed batch, sequence
        positions — exactly as it was handed to the adapter.
        """
    from src.core.json_utils import atomic_write_text
    path = self._tree.runs.run_dir(session_id, run_id) / task_prompts.LAUNCH_TEXT_FILENAME
    atomic_write_text(path, prompt)

  async def _prepare_worktree(self, meta: SessionMetadata, run: RunRecord,
                              repo_path: Path) -> tuple[str, str, str, str | None]:
    """Create the Run's isolated worktree from the requested base; records it on the Run.

        An unattended launch starts from the remote's published default branch
        when no base was requested; a requested base is used verbatim. The
        created branch, worktree and canonical base land on the Run record.
        """
    if self.launch_workspace_guard is not None:
      self.launch_workspace_guard(repo_path)
    remote_tip: str | None = None
    if run.base_branch:
      base_branch = run.base_branch
    elif meta.task is not None and meta.task.base_branch:
      base_branch = meta.task.base_branch
    else:
      default_branch, remote_tip = await git.git_remote_default_branch_and_tip(repo_path)
      base_branch = f"origin/{default_branch}"
    branch_name = run.branch_name or f"charliebot/task-{int(time.time())}-{run.id[:8]}"
    wt_path = Path(self._cfg.paths.worktree_dir) / git.git_worktree_dir_name(branch_name)
    Path(self._cfg.paths.worktree_dir).mkdir(parents=True, exist_ok=True)
    resolution = await git.git_create_worktree(repo_path, base_branch, branch_name, wt_path, remote_tip=remote_tip)
    # The recorded base is the REQUESTED verification target exactly as
    # asked (an origin/ target verifies against the published tip); the
    # worktree's canonical start point stays in the creation resolution.
    await self._tree.runs.record_observation(
        meta.id,
        run.id,
        repo_path=str(repo_path),
        base_branch=base_branch,
        branch_name=branch_name,
        worktree_path=str(wt_path.resolve()),
    )
    return base_branch, branch_name, str(wt_path.resolve()), resolution.start_point

  # ------------------------------------------------------------------
  # Resume interface (the startup-recovery stage's re-attach entry)
  # ------------------------------------------------------------------

  async def resume_run(self, session_id: str, run_id: str, *, is_alive: Callable[[], bool] | None = None) -> None:
    """Re-attach one launched Run and follow it to its terminal fact.

        The startup-recovery pass consumes this interface. Liveness comes from
        the caller's (pid, pid_start) judgment — when omitted, the run's
        recorded identity is judged against the host on every poll (a
        re-evaluable probe, never a captured boolean). Manager turns re-attach
        through the same per-session queue (the follow drains before any
        queued turn spawns); worker runs re-attach through Worker.resume's
        tail-follow. Both land on the same finalize glue as a fresh run. A Run
        that already carries a terminal fact returns without a follow here —
        the recovery pass re-drives missing follow-ups for terminal Runs
        separately, so a Run whose process ended before its follow-up ran is
        never skipped forever.

        While the follow runs, the (session, run) pair sits in
        ``_resume_follows`` — the end-landing retry's node reconcile pass skips
        runs this process is already driving. A worker follow releases the
        pair when this method's follow finishes; a manager-turn follow releases
        only when its master-queue future resolves (the follow outlives this
        call). A process dead on entry is a drain: its ``ended_at`` is the raw
        log's last write time, not the landing write time.
        """
    # The follow pair is registered for the whole call — including the
    # re-checks, so a caller's pre-registered background follow never outlives
    # an early return — with one release point below, unless the manager-turn
    # future's done-callback took the ownership (a manager-turn follow
    # outlives this call).
    key = (session_id, run_id)
    self._resume_follows.add(key)
    future_owned = False
    try:
      future_owned = await self._resume_run_checked(session_id, run_id, is_alive)
    finally:
      if not future_owned:
        self._resume_follows.discard(key)

  async def _resume_run_checked(self, session_id: str, run_id: str, is_alive: Callable[[], bool] | None) -> bool:
    """resume_run's checks and follow, under the caller's follow-pair
    registration; True when the manager-turn future's done-callback took over
    the caller's release."""
    tree = self._tree
    run = await tree.runs.get_run(session_id, run_id)
    if run is None:
      raise RunNotFoundError(run_not_found_in_task_text(run_id, session_id))
    if run.pid is None or run.pid_start is None:
      raise TaskInvalidError(f"run {run_id} records no launched process identity; nothing to re-attach")
    events = tree.runs.load_events_sync(session_id)
    if tree.runs.run_has_terminal_fact(run, events):
      return False
    if is_alive is None:
      # The re-evaluable probe, not a captured boolean: the follow must
      # observe the matching process ENDING (pid reuse and descendants
      # that kept stdout are judged by the existing identity rules on
      # every poll), and a stopped/ended process must converge to its
      # durable result instead of following forever.
      from src.core.runs import read_host_boot_time, run_alive_probe
      is_alive = run_alive_probe(run.pid, run.pid_start, run.started_at, read_host_boot_time())
    meta = await tree.load_meta(session_id)
    if meta is None:
      raise TaskNotFoundError(f"task {session_id} not found")
    alive_on_entry = is_alive()
    ended_at: datetime | None = None
    if alive_on_entry:
      # A re-attached live Run re-marks the busy interval the restart
      # dropped: a worker node's thinking_since re-opens at the Run's
      # recorded started_at (a manager turn's re-attach re-marks through
      # the master queue's own resume enqueue). A drain (is_alive False)
      # converges straight to the durable terminal fact and marks nothing.
      await tree.runs.notify_liveness(session_id, run, launched=True)
    else:
      # The drain rule: the process ended unseen, so its end time is the raw
      # log's last write — never the landing write time. An absent raw log
      # keeps the write-time default (ended_at stays None).
      ended_at = runs.raw_completion_time(tree.runs.run_dir(session_id, run_id) / runs.RAW_LOG_NAME)
    option = self._resolve_run_backend(run)
    if meta.profile == "manager" and run.kind == "manager_turn":
      future = await self._resume_manager_turn(meta, run, option, is_alive, ended_at=ended_at)
      # Never awaited: boot reconcile must not wait on the master queue. The
      # done-callback releases the follow pair and routes an out-of-space
      # follow failure to the end-landing retry.
      self._track_manager_follow(session_id, run_id, future)
      return True
    if meta.profile == "worker" and run.kind in WORKER_KINDS:
      await self._resume_worker_run(meta, run, option, is_alive, ended_at=ended_at)
      return False
    raise TaskInvalidError(f"run {run_id} (kind={run.kind}) has no resume adapter")

  async def _resume_manager_turn(
      self,
      meta: SessionMetadata,
      run: RunRecord,
      option: BackendOption,
      is_alive: Callable[[], bool],
      *,
      ended_at: datetime | None,
  ) -> asyncio.Future:
    """Re-attach a v2 manager turn through the per-session queue's follow path.

        Returns the future the queue consumer resolves with the followed
        turn's result: the caller keeps it (never awaits it) so the follow's
        out-of-space failure can reach the end-landing retry after this call
        has returned.
        """
    from src.agents.master_cc import enqueue_master_resume
    from src.agents.master_cc_state import MasterRunRecord, TaskRunBinding

    transport_dir = self._tree.runs.run_dir(meta.id, run.id)
    record = MasterRunRecord(
        pid=run.pid,
        pid_start=run.pid_start,
        started_at=run.started_at,
        raw_log=str(transport_dir / runs.RAW_LOG_NAME),
    )

    async def on_task_spawn(pid: int, pid_start: str | None) -> None:
      raise RuntimeError(f"resume follow of run {run.id} must not spawn a process")

    on_task_finish = self._manager_finish_recorder(meta.id, run.id, option, transport_dir, ended_at=ended_at)

    return await enqueue_master_resume(
        self._cfg,
        meta,
        record,
        self._sessions.callbacks(),
        is_alive=is_alive,
        task_run=TaskRunBinding(session_id=meta.id, run_id=run.id, transport_dir=str(transport_dir)),
        on_task_spawn=on_task_spawn,
        on_task_finish=on_task_finish,
        extra_env=self._child_env(meta.id, run.id, meta.name),
    )

  async def _resume_worker_run(
      self,
      meta: SessionMetadata,
      run: RunRecord,
      option: BackendOption,
      is_alive: Callable[[], bool],
      *,
      ended_at: datetime | None,
  ) -> None:
    """Re-attach a worker Run through Worker.resume's tail-follow.

        ``ended_at`` carries the drain rule's end time (the raw log's last
        write) for a process dead on entry; a live re-attach passes None and
        keeps the observed-exit write time.
        """
    session_id, run_id = meta.id, run.id
    run_dir = self._tree.runs.run_dir(session_id, run_id)
    events_log = run_dir / RUN_EVENTS_NAME
    binding = RunWorkerBinding(
        id=run_id,
        session_id=session_id,
        pid=run.pid,
        pid_start=run.pid_start,
        claude_session_id=run.native_session_id if option.type == BackendType.CC_CLAUDE else None)
    worker = Worker(
        binding,  # type: ignore[arg-type]
        Path(run.worktree_path) if run.worktree_path else run_dir,
        events_log,
        "",  # the follow builds nothing: the prompt text was the launch's
        self._cfg,
        backend_option=option,
    )
    # Session-level notices reach the session chat exactly as a fresh run's do.
    worker.on_session_event = functools.partial(self._sessions.deliver_to_successor, session_id)
    exit_code = await worker.resume(is_alive=is_alive, on_silence=None)
    durable_outcome = await self._finalize_worker_run(
        meta, run, option, exit_code=exit_code, error="", ended_at=ended_at)
    await self._after_worker_run(meta, run, durable_outcome)

  async def repair_end_metadata(self, session_id: str, run: RunRecord, outcome: str) -> None:
    """Fill a terminal Run's half-written end metadata (the node reconcile
    pass's step-3 repair; the boot reconcile repairs it the same way).

        The ``run_finished`` fact landed but its metadata write failed (out of
        space), so ``ended_at``/``exit_code`` never reached metadata.json. Both
        are re-derived from the raw log by the drain rule — the result scan's
        exit code, the last write time — and ``finish_run``'s repeat path fills
        only the empty fields; values already written stay unchanged. The
        outcome argument is advisory only: the durable fact is authoritative.
        """
    raw_path = self._tree.runs.run_dir(session_id, run.id) / runs.RAW_LOG_NAME
    ended_at = runs.raw_completion_time(raw_path)
    exit_code = -1
    if raw_path.is_file():
      option = self._resolve_run_backend(run)
      _events, _result, scanned = await asyncio.to_thread(
          runs.scan_result_exit, raw_path, self._fresh_translate(option))
      exit_code = scanned
    await self._tree.dispatch.finish_run(session_id, run.id, outcome=outcome, exit_code=exit_code, ended_at=ended_at)

  # ------------------------------------------------------------------
  # Post-finish delivery chain
  # ------------------------------------------------------------------

  async def _after_worker_run(self, meta: SessionMetadata, run: RunRecord, durable_outcome: str) -> None:
    """The delivery chain one worker Run's durable outcome drives.

        A work Run finishing is not delivery: an implement task's review and
        target-branch landing must complete first, and a failed or blocked
        outcome reports the parent with durable stable evidence while the task
        and its worktree stay available for the explicit retry. Sequence Runs
        (iteration, scheduled_step) have no delivery chain of their own: their
        sequence controller owns progression and the one final report, so this
        method only re-enters the node's own dispatcher for inputs the Run left
        pending.
        """
    session_id = meta.id
    try:
      if run.kind == "scheduled_step":
        # The firing's owning module re-drives the frontier from this
        # durable finish: the next permitted step launches, or the ONE
        # boundary report re-delivers — without waiting for the next
        # tick or restart, and idempotent against a live controller.
        await self._redrive_firing(session_id)
        return
      if run.kind == "iteration":
        return
      if run.kind == "review":
        await self._after_review_run(meta, run, durable_outcome)
        return
      if durable_outcome == "success":
        task_type = meta.task.task_type if meta.task is not None else TaskType.IMPLEMENT
        if task_type == TaskType.IMPLEMENT:
          await self._maybe_spawn_review(session_id, run)
        else:
          await self._cleanup_worktree_if_delivered(session_id, run)
        return
      if durable_outcome == "failed":
        await self._report_failure_to_parent(session_id, run, durable_outcome)
    finally:
      # Inputs admitted during this run waited for the serialized
      # consumer; the finish chain (review queued/landed, closure,
      # failure report) ran first, so this dispatch only sees what that
      # chain left pending. The review path reaches it too, so the
      # inputs admitted during a review Run dispatch here as well.
      await self._tree.dispatch.dispatch_pending(session_id)

  async def _redrive_firing(self, session_id: str) -> None:
    """Re-drive one scheduled firing from its leaf's durable facts."""
    from src.core.cron_sequence import redrive_firing

    await redrive_firing(session_id, self._tree, self._cfg)

  async def _after_review_run(self, meta: SessionMetadata, run: RunRecord, durable_outcome: str) -> None:
    from src.core.task_completion import (
        LANDING_REF_PREFIX,
        REVIEW_REF_PREFIX,
        RUN_REF_PREFIX,
        SPEC_REF_PREFIX,
        CompletionEvidence,
        LandingEvidence,
    )
    from src.core.task_sessions import TaskConflictError

    session_id = meta.id
    work_run = await self._tree.runs.get_run(session_id, run.review_of_run_id or "")
    if work_run is None:
      raise TaskInvalidError(f"review run {run.id} names no recorded work Run")
    if durable_outcome == "failed":
      retried = await self._maybe_spawn_review(session_id, work_run)
      if retried is None:
        await self._report_failure_to_parent(
            session_id,
            work_run,
            "blocked",
            summary=f"review of work run {work_run.id} failed on every configured reviewer backend")
      return
    if durable_outcome != "success":
      return
    task_state = self._tree.task_state(session_id)
    if task_state != "open":
      # A closed task already consumed this review's verdict: its recorded
      # close replays the automatic-completion call before reading the
      # evidence, but the landing proof still shells out to git (rev-parse,
      # fetch, cat-file, merge-base) once per closed node on every startup
      # reconcile. Only the worktree cleanup still runs; a reopened task
      # derives "open" again and gets the full proof.
      log.debug(
          "review_followup_skipped_closed_task",
          session_id=session_id,
          run_id=run.id,
          work_run=work_run.id,
          task_state=task_state)
      await self._cleanup_worktree_if_delivered(session_id, work_run)
      return
    refs = [f"{RUN_REF_PREFIX}{work_run.id}"]
    if work_run.task_spec_hash:
      refs.append(f"{SPEC_REF_PREFIX}{work_run.task_spec_hash}")
    refs.append(f"{REVIEW_REF_PREFIX}{run.id}")
    if work_run.repo_path:
      # A repo task's implement delivery lands its reviewed branch on the
      # base; an unproven landing is a blocked report, not a close.
      landing, reason = await self._landing_for_work(work_run)
      if landing is None:
        await self._report_failure_to_parent(
            session_id,
            work_run,
            "blocked",
            summary=f"work run {work_run.id} passed review but its branch did not land on "
            f"{work_run.base_branch or 'the requested base'}: {reason}")
        return
      branch, commit, repo_path = landing
      refs.append(f"{LANDING_REF_PREFIX}{branch}@{commit}")
      evidence = CompletionEvidence(
          summary=f"work run {work_run.id} delivered after review {run.id} landed {commit[:12]} on {branch}",
          result_refs=refs,
          run_ids=[work_run.id],
          review_run_ids=[run.id],
          landing=LandingEvidence(branch=branch, commit=commit, repo_path=repo_path),
      )
    else:
      # A repo-less task's implement delivery is the reviewer's verdict
      # on the reported paths; no landing exists to prove.
      evidence = CompletionEvidence(
          summary=f"work run {work_run.id} delivered after review {run.id} "
          "checked the reported paths against the acceptance tests",
          result_refs=refs,
          run_ids=[work_run.id],
          review_run_ids=[run.id],
          landing=None,
      )
    try:
      await self._tree.completion.evaluate_automatic_completion(session_id, run_id=work_run.id, evidence=evidence)
    except TaskConflictError as e:
      log.warning(
          "task_delivery_close_blocked",
          session_id=session_id,
          run_id=work_run.id,
          blockers=getattr(e, "blockers", None))
    finally:
      await self._cleanup_worktree_if_delivered(session_id, work_run)

  async def _maybe_spawn_review(self, session_id: str, work_run: RunRecord) -> str | None:
    """Spawn the work Run's review on the same task, repo, branch and worktree.

        Every successful implement work Run gets one; a repo-less work Run's
        review judges the reported paths (no repo, branch or worktree to
        carry). The one owner of "does this work Run still need a review":
        finalize, the recovery replay and the failed-review retry all enter
        here, so the chain ends at the first successful review everywhere. An
        existing successful review ends the chain — its id returns and no
        further review Run is ever registered for this work Run, on any number
        of restarts (a review that succeeded without proving its landing
        follows the landing and blocked-report path; it is never re-reviewed).
        An existing non-terminal review means one is already queued or running
        (recovery and repeated finalize never spawn a second). A failed
        reviewer retries down the existing preference policy with distinct Run
        records; exhausted retries keep the worktree (a repo task's) and
        report blocked.
        """
    tree = self._tree
    async with tree.control_lock:
      events = tree.runs.load_events_sync(session_id)
      if tree.runs.terminal_outcome(events, work_run.id) != "success":
        return None
      # The caller's in-memory record predates the launch's observation
      # writes (worktree facts, native id): the durable record is the
      # review's provenance source.
      fresh = await tree.runs.get_run(session_id, work_run.id)
      if fresh is not None:
        work_run = fresh
      existing = [
          r for r in tree.runs.list_run_records_sync(session_id)
          if r.kind == "review" and r.review_of_run_id == work_run.id
      ]
      # The chain ends at the first successful review: its terminal fact
      # is the settled verdict, never a used attempt that would select
      # the next reviewer backend.
      for existing_review in existing:
        if tree.runs.terminal_outcome(events, existing_review.id) == "success":
          return existing_review.id
      if any(not tree.runs.run_has_terminal_fact(r, events) for r in existing):
        return existing[0].id
      attempts = len(existing)
      selection = review.select_reviewer_backend(
          self._cfg, work_run.backend or "", work_run.model, [r.backend for r in existing if r.backend])
      if selection is None:
        log.warning("reviewer_backends_exhausted", session_id=session_id, work_run=work_run.id)
        return None
      resolved_backend, resolved_model, _tried = selection
      run_id = stable_run_id(session_id, f"review:{work_run.id}:{attempts + 1}")
      record = RunRecord(
          id=run_id,
          session_id=session_id,
          kind="review",
          review_of_run_id=work_run.id,
          backend=resolved_backend,
          model=resolved_model,
          repo_path=work_run.repo_path,
          base_branch=work_run.base_branch,
          branch_name=work_run.branch_name,
          worktree_path=work_run.worktree_path,
      )
      await tree.runs.register_run_locked(record, task_spec_text=f"review of work run {work_run.id}")
    self.launch(session_id, run_id)
    return run_id

  async def _landing_for_work(self, work_run: RunRecord) -> tuple[tuple[str, str, str] | None, str]:
    """The (branch, commit, repo) landing evidence of a reviewed work Run, or None and why.

        The commit is the work branch's tip in its repository; the check
        requires that commit to exist and be an ancestor of the branch the
        reviewer's push publishes (review.review_landing_target, fetched
        first), never the possibly stale local branch of the same name.
        """
    if not (work_run.repo_path and work_run.branch_name and work_run.base_branch):
      return None, f"work run {work_run.id} records no repo, branch and base to verify"
    commit = await git.git_rev_parse(Path(work_run.repo_path), work_run.branch_name)
    if commit is None:
      return None, f"branch {work_run.branch_name} does not resolve in {work_run.repo_path}"
    target = review.review_landing_target(work_run.base_branch)
    from src.core.git import git_verify_commit_landed
    landed, reason = await git_verify_commit_landed(Path(work_run.repo_path), target, commit)
    if not landed:
      return None, reason
    return (target, commit, work_run.repo_path), ""

  async def _cleanup_worktree_if_delivered(self, session_id: str, work_run: RunRecord) -> None:
    """Remove the shared worktree only once the task actually delivered.

        Failed, blocked and unproven outcomes keep the worktree; keep_worktree
        pins it; a task that did not close keeps it for the explicit retry.
        """
    meta = await self._tree.load_meta(session_id)
    if meta is None or meta.task is None or meta.task.keep_worktree:
      return
    if self._tree.task_state(session_id) != "completed":
      return
    await git.git_worktree_remove_reporting(
        work_run.repo_path,
        work_run.worktree_path,
        work_run.branch_name,
        work_run.id,
        Path(self._cfg.paths.worktree_dir),
        log_fields={
            "run_id": work_run.id,
            "session": session_id
        },
        label="Task worktree",
        fail_event="task_worktree_cleanup_failed",
        remove_failed_event="task_worktree_remove_failed",
    )

  async def _report_failure_to_parent(
      self, session_id: str, run: RunRecord, outcome: str, *, summary: str | None = None) -> None:
    """Persist one failed/blocked child_report with stable source/recipient evidence.

        The source event is the Run's durable run_finished fact and the
        recipient is the close-time fixed parent, so the stable report id
        dedups across recovery and repeated finalize without ever relying on
        the legacy master_woke_after_summary judgment. Only a freshly created
        report wakes the parent, so a recovery re-delivery of an already
        delivered report never wakes twice.
        """
    from src.core.task_completion import RUN_REF_PREFIX
    meta = await self._tree.load_meta(session_id)
    if meta is None or not meta.task_parent_id:
      return
    events = self._tree.runs.load_events_sync(session_id)
    source = next((e for e in reversed(events) if e.get("type") == ET.RUN_FINISHED and e.get("run_id") == run.id), None)
    if source is None:
      return
    if summary is None:
      summary = await self._worker_failure_summary(session_id, run)
    # The delivery entry owns the wake: a freshly created report wakes the
    # parent's next serialized turn (dispatcher for a task-tree parent, the
    # legacy master wake for a legacy parent); a replayed one wakes nobody.
    await self._tree.dispatch.deliver_child_report(
        session_id,
        source_event=source,
        outcome=outcome,
        summary=summary,
        result_refs=[f"{RUN_REF_PREFIX}{run.id}"],
        recipient=meta.task_parent_id,
    )

  async def _worker_failure_summary(self, session_id: str, run: RunRecord) -> str:
    """The failed run's own closing words (or its error, or the bare outcome).

        The summary must name the actual error when the run produced no report:
        a run that died before its process started carries its error as the
        events log's error event, and that text is the report's evidence.
        """
    events_log = self._tree.runs.run_dir(session_id, run.id) / RUN_EVENTS_NAME
    if events_log.is_file():
      text = await asyncio.to_thread(review._worker_summary_from_events_log, events_log)
      if text:
        return text
      error_text = await asyncio.to_thread(review._worker_error_from_events_log, events_log)
      if error_text:
        return error_text
    return f"run {run.id} failed without a reportable output"


# ---------------------------------------------------------------------------
# TUI terminal launch (the v2 startup context boundary's terminal edge)
# ---------------------------------------------------------------------------

# In-process launch guard for TUI terminal launches: one prepare per task per
# process, spanning registration to the tmux ensure. The durable (pid,
# pid_start) identity write is the cross-restart backstop, exactly like the
# headless launch guard.
_TUI_LAUNCH_INFLIGHT: set[str] = set()

# How long a second attach waits for the in-flight owner's launch window before
# refusing to guess: it must never fall through to the legacy uncredentialed
# ensure while the owner's Run credential and snapshot are still being
# delivered.
_TUI_LAUNCH_WAIT_SECONDS = 15.0


@dataclasses.dataclass
class TuiTaskLaunch:
  """One scripted-or-real terminal launch's committed context.

    Carries exactly what the terminal launch needs: the Run's id, the native
    conversation id (hash-qualified), the injected child environment (its own
    identity and signed run credential — never the operator key), and the
    snapshot's instruction bytes for the instruction file the terminal claude
    reads. ``record_process`` pins the pane process identity onto the Run with
    the same owners a headless launch uses.
    """

  session_id: str
  run_id: str
  native_session_id: str
  inject_env: dict[str, str]
  instructions_text: str
  working_dir: Path
  model: str | None
  _tree: TaskTreeManager

  async def record_process(self) -> None:
    """Pin the terminal's process identity (tmux pane pid + start marker) on the Run."""
    from src.agents.backends.pty_common import tmux_pane_pid
    from src.core.runs import read_pid_stat

    pid = await tmux_pane_pid(self.session_id)
    if pid is None:
      raise RuntimeError(f"TUI launch of run {self.run_id} created no tmux pane to pin a pid from")
    stat = read_pid_stat(pid)
    if stat is None:
      raise RuntimeError(
          f"TUI launch of run {self.run_id}: pane pid {pid} has no /proc entry; "
          "refusing to pin a start marker that cannot be verified")
    pid_start = stat[0]
    await self._tree.runs.record_launch(self.session_id, self.run_id, pid=pid, pid_start=pid_start)
    await self._tree.runs.record_observation(self.session_id, self.run_id, native_session_id=self.native_session_id)


async def fail_unlaunched_tui_run(
    tree: TaskTreeManager,
    session_id: str,
    run_id: str,
    *,
    reason: str,
) -> None:
  """Land the definitely-unlaunched terminal fact for a prepared TUI launch.

    Nothing started and the Run never claimed an input batch, so it records
    ``failed`` — never a ghost queued Run that blocks the node's structural
    operations forever. The fact lands only while no process identity is
    pinned: a pinned pane means a live process exists and the explicit stop
    owns its ending. Best-effort: a failure here logs and never masks the
    launch failure it reports.
    """
  try:
    events = tree.runs.load_events_sync(session_id)
    run = await tree.runs.get_run(session_id, run_id)
    if run is None or run.pid is not None or tree.runs.run_has_terminal_fact(run, events):
      return
    await tree.runs.record_finish(session_id, run_id, "failed")
    log.warning("tui_task_launch_marked_failed", session_id=session_id, run_id=run_id, reason=reason)
  except Exception:
    log.error("tui_task_launch_failure_fact_failed", session_id=session_id, run_id=run_id, reason=reason, exc_info=True)


async def prepare_tui_task_launch(
    cfg: CharlieBotConfig,
    session_id: str,
    tree: TaskTreeManager | None,
) -> TuiTaskLaunch | None:
  """The v2 TUI task's launch seam: one Run per actual terminal launch, or None.

    Returns None when the attach must not launch anything: a v1 session (its
    legacy terminal behavior is untouched), a non-tui backend, or a live
    terminal (a re-attach never creates a Run or relaunches — an ongoing
    terminal keeps its start snapshot). A launch registers the Run, commits its
    instruction snapshot under the same coherence protocol as every other
    launch, and signs the run credential the terminal's CLI will use — never
    the operator key. Native continuation is chosen by instruction hash: the
    native conversation id qualifies the stable task id with the snapshot's
    hash, so an unchanged hash resumes the conversation and a rules/source
    change starts a fresh native context (the earlier transcript stays on
    disk).

    The caller must call :func:`release_tui_launch` once the tmux session is
    ensured (or failed to ensure): the in-process launch marker spans the
    registration-to-tmux window, so two concurrent attaches cannot register two
    Runs for one terminal. A second attach inside that window waits for it
    (bounded) instead of falling through to the legacy uncredentialed ensure.
    A failure after registration discards the marker and lands a ``failed``
    terminal fact on the registered Run, so the next attach retries cleanly
    and the node never keeps a permanently queued ghost Run.
    """
  from src.agents.backends.pty_common import tmux_session_exists
  from src.core.backend_models import BackendType

  if tree is None:
    # The server's singleton owner (the same one every API route uses); the
    # lazy import keeps this core module off the API modules' import path.
    from src.api.deps import task_manager
    tree = task_manager()
  deadline = time.monotonic() + _TUI_LAUNCH_WAIT_SECONDS
  while True:
    if await tmux_session_exists(session_id):
      return None  # a re-attach: the ongoing terminal keeps its start snapshot
    async with tree.control_lock:
      meta = await tree.load_meta(session_id)
      if meta is None or meta.profile is None:
        return None  # not a task-tree node: legacy terminal behavior
      option = cfg.get_backend_option(meta.backend) if meta.backend else None
      if option is None or option.type is not BackendType.TUI_CLI:
        return None
      if meta.profile != "manager":
        raise TaskInvalidError(f"task {session_id}: a tui terminal launch requires a manager node")
      owner_in_flight = session_id in _TUI_LAUNCH_INFLIGHT
      if not owner_in_flight:
        events = tree.runs.load_events_sync(session_id)
        runs = tree.runs.list_run_records_sync(session_id)
        if any(r.kind == "manager_turn" and r.pid is not None and not any(
            e.get("type") == ET.RUN_FINISHED and e.get("run_id") == r.id for e in events) for r in runs):
          return None  # a live terminal Run already owns this task's terminal
        _TUI_LAUNCH_INFLIGHT.add(session_id)
        launch_no = 1 + len([r for r in runs if r.kind == "manager_turn"])
        run_id = stable_run_id(session_id, f"tui-launch:{launch_no}")
        record = RunRecord(
            id=run_id, session_id=session_id, kind="manager_turn", backend=meta.backend, model=option.model)
        await tree.runs.register_run_locked(record, task_spec_text=canonical_task_spec_text(meta.task))
    if not owner_in_flight:
      break
    if time.monotonic() > deadline:
      raise TaskInvalidError(f"task {session_id}: another TUI terminal launch is still in flight")
    await asyncio.sleep(0.05)
  try:
    # Snapshot assembly and persistence run outside the control lock (they
    # take their own short holds); the bytes are committed before the tmux
    # session exists, so the launched claude reads the saved bytes.
    snapshot, _overlay_error, _declared = await assemble_coherent_snapshot(cfg, tree, meta, "manager_turn", option)
    snapshot_path = tree.runs.run_dir(session_id, run_id) / task_prompts.SNAPSHOT_FILENAME
    from src.core.json_utils import atomic_write_text
    await asyncio.to_thread(
        atomic_write_text, snapshot_path, json.dumps(snapshot.to_json_dict(), indent=2, ensure_ascii=False))
    await tree.runs.record_observation(session_id, run_id, prompt_snapshot_ref=str(snapshot_path))
    key = configured_access_key()
    if not key:
      raise RuntimeError(
          "run-token signing requires credentials.yaml charliebot.access_key; "
          "a TUI task launch cannot inject its run credential without it")
    token = sign_run_token(RunTokenClaims(session_id=session_id, run_id=run_id, agent=meta.name or "manager"), key)
    return TuiTaskLaunch(
        session_id=session_id,
        run_id=run_id,
        native_session_id=f"{session_id}-{snapshot.prompt_hash[:8]}",
        inject_env={
            SESSION_ID_ENV_VAR: session_id,
            RUN_TOKEN_ENV: token,
            "CHARLIEBOT_HOME": str(cfg.charliebot_home),
        },
        instructions_text=snapshot.instructions_text,
        working_dir=cfg.sessions_dir / session_id,
        model=option.model,
        _tree=tree,
    )
  except BaseException as e:
    release_tui_launch(session_id)
    await fail_unlaunched_tui_run(tree, session_id, run_id, reason=str(e))
    raise


def release_tui_launch(session_id: str) -> None:
  """Drop the in-process TUI launch marker once the tmux ensure settled."""
  _TUI_LAUNCH_INFLIGHT.discard(session_id)
