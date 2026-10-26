"""The task-tree execution adapter: binds v2 Runs to the real backend processes.

This module is the execution-glue owner of the task tree. It holds the
one :class:`TaskExecutionAdapter` the application installs as the
:class:`~src.runtime.session_dispatch.TaskInputDispatcher` executor, plus the
common launch/resume interfaces the sequence controllers, the delayed
triggers, and startup recovery consume.

Ownership boundaries it never crosses:

- Input admission, dedup and batch claims stay in
  :mod:`src.runtime.session_dispatch`; run records, terminal facts and stop
  requests stay in :mod:`src.runtime.runs`; task metadata and tree relations stay
  in :mod:`src.runtime.task_sessions`; completion/evidence policy stays in
  :mod:`src.runtime.task_completion`. This module only binds a registered Run to
  the existing process harnesses — the master queue (``src.runtime.master_cc_queue``)
  for manager turns, the Worker/backend adapters (``src.runtime.worker``) for
  work and review — and lands their outcomes through those owners.
- The one short control write lock is held only across durable reservation
  and identity writes. Backend, git, network and model work run outside it.
- A Run is the sole execution record: no second writable ``ThreadMetadata``
  file is written for a launch.

Serialization evidence: concurrent dispatch calls reserve the consumer under
the control lock — a fresh consumer binds the exact pending batch through
``claim_input_batch_locked`` inside that hold, so the second caller's claim
comes back empty and it never spawns; a queued Run is launched through the
in-process launch guard so two dispatch calls cannot both start it. The spawned process identity
(``pid`` + ``pid_start``) lands on the Run before any call from its credential
is accepted (:meth:`RunStore.record_launch` runs inside the backend's
on_spawn callback, and the caller-identity dependency requires both fields).
"""

from __future__ import annotations

import asyncio
import dataclasses
import datetime
import errno
import functools
import hashlib
import json
import pathlib
import shutil
import time
import uuid
from collections.abc import Awaitable, Callable

from src.infra import (
    config,
    constants,
    git,
    json_utils,
    log_once,
    metadata_slots,
    models,
    ndjson,
    tasks,
)
from src.infra import event_types as ET
from src.runtime import (
    backend_pool,
    chat_events,
    cold_read,
    control_events,
    master_cc_queue,
    master_cc_state,
    midturn_delivery,
    review,
    run_token,
    runs,
    session_anchors,
    session_events,
    session_fork,
    session_lifecycle,
    session_listing,
    session_sidebar,
    session_store,
    session_successor,
    spawner_backends,
    task_completion,
    task_errors,
    task_input_prompt,
    task_prompts,
    task_sessions,
    thinking_state,
    worker,
)
from src.runtime.hooks import backend_lifecycle, backend_types, sequence_controllers

log = log_once.LazyStructlogLogger()


def run_worker_binding(
    run_id: str,
    session_id: str,
    *,
    pid: int | None = None,
    pid_start: str | None = None,
) -> models.ThreadMetadata:
  """The Run-backed worker identity a v2 launch hands to :class:`~src.runtime.worker.Worker`.

    An in-memory thread record that is never written: the Worker reads its id,
    session id, process identity and the backend's preassigned conversation id.
    The pid/pid_start pair is persisted onto the Run by the adapter's
    ``on_spawned`` callback, not onto a thread file.
    """
  return models.ThreadMetadata(id=run_id, session_id=session_id, description="", pid=pid, pid_start=pid_start)


def _native_continuation_matches(
    held_id: str | None,
    recorded_hash: str | None,
    recorded_backend: str | None,
    recorded_model: str | None,
    *,
    prompt_hash: str,
    option: models.BackendOption,
) -> bool:
  """The one native-conversation continuation rule, shared by manager turns and worker runs.

  A held conversation continues only when the recorded prompt hash, backend
  identity and model all match this launch's own; the same backend identity
  always shares the launch's continuation domain.
  """
  return (
      held_id is not None and recorded_hash == prompt_hash and recorded_backend == option.id and
      recorded_model == option.model)


def _snapshot_prompt_hash(run: models.RunRecord) -> str | None:
  """The prompt hash of one launched Run's committed snapshot, or None for a Run that never committed one."""
  if run.prompt_snapshot_ref is None:
    return None
  return str(json.loads(pathlib.Path(run.prompt_snapshot_ref).read_text(encoding="utf-8"))["prompt_hash"])


# The reset reason when the previous Run recorded no native conversation id, so there is nothing to resume.
NO_NATIVE_ID_RESET_REASON = (
    "the previous run recorded no native conversation id, so this run starts a fresh native conversation")


def worker_continuation_reset_note(reason: str, previous_log_path: pathlib.Path) -> str:
  """The bracketed note that opens a continued worker node's fresh native conversation.

  Trade-off 1 of the pause/continue plan: the branch and worktree are
  inherited and the task spec with the new input follows, so the note names
  where the previous Run's log stays readable — the fresh context's only way
  to recover what the earlier conversation already did.
  """
  return (
      f"{session_anchors.CONTEXT_RESET_OPENER}{reason}. The task continues on the same branch and worktree; "
      f"its spec and the new input follow. The previous run's log is at {previous_log_path} — "
      "read it before acting, so finished work is not repeated.]")


@dataclasses.dataclass(frozen=True)
class RunContinuation:
  """One worker-node continuation: the previous Run and whether its native conversation continues.

  ``native_id`` is the previous Run's native session id when the continuation
  rule matched (the prompt carries only the new input batch); None means the
  branch and worktree are inherited but the conversation starts fresh.
  """
  previous: models.RunRecord
  native_id: str | None


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


def closed_task_stop_request_id(run_id: str) -> str:
  """The stop-request id settling a never-launched Run on a closed node.

  The launch path's closed-task withhold and the startup reconcile derive
  the same id, so a Run both paths settle still carries exactly one
  run_stop_requested fact.
  """
  return f"closed-task:{run_id}"


def free_disk_gib(path: pathlib.Path) -> float:
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


def resolve_launch_overlay(option: models.BackendOption) -> tuple[str | None, bool]:
  """The three-state overlay judgment used by a task launch.

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
    tree: task_sessions.TaskTreeManager,
    index: object,
    meta: models.SessionMetadata,
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


async def _capture_chain_under_control_lock(
    tree: task_sessions.TaskTreeManager,
    meta: models.SessionMetadata,
) -> tuple[tuple[tuple[str, str | None], ...], str | None, models.SessionMetadata]:
  """Reload *meta* and capture its prompt chain inside one control-lock hold.

  The index build, the meta reload, and the ancestor walk are one unit: the
  chain is only coherent with the body the caller builds from the returned
  meta when all three share the same lock hold. A task that vanished
  mid-assembly fails loud instead of assembling from a half-deleted node.
  """
  async with tree.control_lock:
    index = await tree._get_index()
    fresh_meta = await tree.load_meta(meta.id)
    if fresh_meta is None:
      raise task_errors.TaskNotFoundError(f"task {meta.id} vanished during prompt assembly")
    chain, node_ref = capture_prompt_chain(tree, index, fresh_meta)
    return chain, node_ref, fresh_meta


async def assemble_coherent_snapshot(
    cfg: config.CharlieBotConfig,
    tree: task_sessions.TaskTreeManager,
    meta: models.SessionMetadata,
    kind: str,
    option: models.BackendOption,
) -> tuple[task_prompts.PromptSnapshot, OSError | None, bool]:
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
  snapshot: task_prompts.PromptSnapshot | None = None
  overlay_error: OSError | None = None
  for _ in range(task_prompts._COHERENCE_PASSES):
    chain, node_ref, fresh_meta = await _capture_chain_under_control_lock(tree, meta)
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
    chain2, node_ref2, _ = await _capture_chain_under_control_lock(tree, meta)
    if (chain2, node_ref2) != (chain, node_ref):
      continue  # a rule/ancestor moved: rebuild from the new view
    snapshot, overlay_error = candidate, (err or err2)
    break
  if snapshot is None:
    raise task_prompts.TaskPromptError(
        f"prompt sources for task {meta.id} did not settle across "
        f"{task_prompts._COHERENCE_PASSES} coherent-view attempts")
  return snapshot, overlay_error, declared


async def apply_sequence_wake_duties(meta: models.SessionMetadata, input_events: list[dict]) -> str | None:
  """Run the registered sequence binding's wake duties, if the node has one."""
  binding = sequence_controllers.binding_for(meta.id)
  if binding is None:
    return None
  return await binding.on_wake(meta, input_events)


class TaskExecutionAdapter:
  """Binds registered Runs to the existing master/worker execution harnesses."""

  def __init__(
      self,
      cfg: config.CharlieBotConfig,
      successor: session_successor.SessionSuccessor,
      events: session_events.SessionEvents,
      lifecycle: session_lifecycle.SessionLifecycle,
      anchors: session_anchors.SessionAnchors,
      sidebar: session_sidebar.SessionSidebar,
      tree: task_sessions.TaskTreeManager,
  ) -> None:
    self._cfg = cfg
    self._successor = successor
    self._events = events
    self._lifecycle = lifecycle
    self._anchors = anchors
    self._sidebar = sidebar
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
    # A worker run that ended on quota exhaustion: its reset moment (None when unnamed), until a
    # failover, review or improve-loop caller takes it.
    self._quota_failures: dict[tuple[str, str], datetime.datetime | None] = {}
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
          raise task_errors.TaskNotFoundError(runs.run_not_found_in_task_text(launch_run_id, session_id))
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
        request_id = "dispatch:" + control_events.sha256_hex("\x00".join(sorted(str(e.get("id")) for e in pending_now)))
        run_id = control_events.stable_run_id(session_id, request_id)
        existing = await tree.runs.get_run(session_id, run_id)
        if existing is None:
          kind = "manager_turn" if meta.profile == "manager" else "work"
          record = models.RunRecord(
              id=run_id,
              session_id=session_id,
              kind=kind,  # type: ignore[arg-type]
              backend=backend,
              model=model)
          if meta.profile == "worker":
            self._inherit_worker_continuation(meta, record)
          # This lock hold already covers the reservation: the locked
          # registration variant, then the locked batch claim.
          await tree.runs.register_run_locked(record, task_spec_text=task_sessions.canonical_task_spec_text(meta.task))
        try:
          bound = await tree.dispatch.claim_input_batch_locked(session_id, run_id)
        except task_errors.TaskConflictError as e:
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

  def _inherit_worker_continuation(self, meta: models.SessionMetadata, record: models.RunRecord) -> None:
    """Bind a fresh dispatch on an already-run worker node to the node's latest Run.

        The continuation inherits the latest Run's repo, base, branch and
        worktree — no new worktree is ever prepared for it — and its kind is
        ``review`` when that latest Run is a successful review whose merge
        never started (the task holds no close fact and never sent a blocked
        report to its parent), ``work`` otherwise. A review continuation runs
        on the paused review's backend and model, so the reviewer that paused
        still judges the work; a work continuation keeps the node default. A
        node whose episode closed (delivered or cancelled) keeps the first-run
        shape: the next run prepares its own worktree from the base.
        """
    session_id = meta.id
    latest = self._tree.runs.newest_run_record_sync(session_id)
    if latest is None:
      return
    if self._tree.facts_of(session_id).close_events:
      return
    record.repo_path = latest.repo_path
    record.base_branch = latest.base_branch
    record.branch_name = latest.branch_name
    record.worktree_path = latest.worktree_path
    events = self._tree.runs.load_events_sync(session_id)
    if (latest.kind == "review" and self._tree.runs.terminal_outcome(events, latest.id) == "success" and
        not self._blocked_report_sent(meta)):
      record.kind = "review"
      record.review_of_run_id = latest.review_of_run_id
      record.backend = latest.backend
      record.model = latest.model

  def _blocked_report_sent(self, meta: models.SessionMetadata) -> bool:
    """True when this node ever delivered a blocked child report to its parent.

        The report fact lives in the recipient's own log (parent-log
        persistence is delivery), so the read is the parent's durable fact
        history; a root node has no recipient and never reported.
        """
    parent = meta.task_parent_id
    if not parent:
      return False
    return any(
        e.get("type") == ET.CHILD_REPORT and e.get("child_session_id") == meta.id and e.get("outcome") == "blocked"
        for e in self._tree.fact_history(parent))

  def _reserve_backend_model(self, meta: models.SessionMetadata) -> tuple[str, str | None]:
    """The task's backend+model for a fresh reservation, resolved strictly."""
    return spawner_backends._resolve_session_default_backend_model(self._cfg, meta)

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

        The delegate path, the review chain, and the sequence controllers all
        enter here; the dispatcher's executor seam lands on the same code
        through :meth:`__call__`. ``prompt`` is the controllers' explicit
        launch text (an iteration's composed description, a scheduled step's
        prompt): the Run's own input batch stays what its registration bound,
        and the override is persisted onto the Run as the launch-text evidence
        before any process starts.
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

    self._landing_retries[session_id] = tasks.create_logged_task(
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
          counters = {"nodes": 0, "resumed": 0, "drained": 0, "followups": 0}
          await _reconcile_node(
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

  def _drives_run(self, session_id: str, run: models.RunRecord) -> bool:
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

    self._resume_follows.add((session_id, run_id))
    tasks.create_logged_task(self._follow_run_background(session_id, run_id), name=f"task-resume-{run_id[:8]}")

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

    tasks.create_logged_task(_execute_and_release(), name=f"task-run-{run_id[:8]}")

  async def _note_launch_out_of_space(self, session_id: str, run_id: str) -> None:
    """Hook 1 of the end-landing retry: the launch path died out of space.

        When the run's process is dead, the worker header timer the finish
        would have closed stays open — close it here (process memory only, no
        disk write) — and start the node's retry task so the node reconcile
        pass drains the run from its raw log and lands the end record once
        space returns.
        """
    run = await self._tree.runs.get_run(session_id, run_id)
    if (run is not None and run.pid is not None and
        not runs.is_run_alive(run.pid, run.pid_start, run.started_at, runs.read_host_boot_time())):
      thinking_state.clear_run_busy(session_id, run_id)
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
        request. There is no authorization re-judgment here — the runtime
        gate is a request-entry check (delegation, the sequence commands,
        agent messages to worker nodes), so review Runs, manual retries,
        startup recovery and scheduled fires are never withheld by later
        conversation. The backend
        resolution afterwards is explicit — a missing or invalid backend fails
        visibly, never silently substituted. ``launch_prompt`` is the sequence
        controllers' explicit launch text; see :meth:`launch`.

        Returns ``LAUNCH_STARTED`` when the Run's execution adapter was
        entered, or the actual refusal reason when a launch precondition
        withheld it (no process started and no terminal fact will arrive). A
        withheld launch records its durable ``run_launch_withheld`` event on
        the node — task closed, a durable stop request, or prompt assembly
        failed — and one blocked child report to the parent (once per run and
        reason, by stable id). A closed-task withhold also settles the
        never-launched Run with its durable stop request (request id
        ``closed-task:<run id>``), the state the archive check and the
        dispatch already read as resolved; every other withhold reason
        leaves the Run queued without one. An exception escaping before the
        adapter was
        entered is a
        failed-to-start launch: the caller's settlement sees it, never an
        endless wait.
        """
    tree = self._tree
    # The withheld verdict (closed node, durable stop, or disk headroom) is
    # recorded after the lock releases: the durable record and the parent
    # report take the lock themselves. The last tuple field marks the
    # closed-task withhold, whose Run the stop request settles below.
    withheld: tuple[models.SessionMetadata, models.RunRecord, str, bool] | None = None
    async with tree.control_lock:
      meta = await tree.load_task_meta(session_id)
      run = await tree.runs.get_run(session_id, run_id)
      if run is None:
        raise runs.RunNotFoundError(runs.run_not_found_in_task_text(run_id, session_id))
      state = tree.task_state(session_id)
      if state != "open":
        reason = f"task {session_id} is {state}"
        log.info("run_launch_withheld", session_id=session_id, run_id=run_id, reason=reason)
        withheld = (meta, run, reason, True)
      else:
        await tree._require_open_ancestry(session_id)
        events = tree.runs.load_events_sync(session_id)
        if tree.runs.run_has_terminal_fact(run, events):
          log.info("run_launch_refused_by_facts", session_id=session_id, run_id=run_id)
          return f"refused: run {run_id} already finished"
        if tree.runs.stop_requested(events, run_id):
          reason = f"run {run_id} has a durable stop request"
          log.info("run_launch_withheld", session_id=session_id, run_id=run_id, reason=reason)
          withheld = (meta, run, reason, False)
        else:
          # Trade-off 1: worker-class runs are the write-heavy launches
          # (environment installs, test runs) — below the configured free-space
          # floor they stay queued with a blocked report instead of dying
          # mid-run. Manager turns always launch: they write little and are the
          # path that reports the shortage.
          if meta.profile == "worker" and run.kind in task_prompts.WORKER_KINDS:
            reason = self._disk_headroom_withhold_reason()
            if reason is not None:
              log.info("run_launch_withheld", session_id=session_id, run_id=run_id, reason=reason)
              withheld = (meta, run, reason, False)
    if withheld is not None:
      meta, run, reason, settle = withheld
      verdict = await self._record_launch_withheld(meta, run, reason)
      if settle:
        await tree.runs.request_stop(session_id, run.id, closed_task_stop_request_id(run.id))
      return verdict
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
    except task_prompts.TaskPromptError as e:
      log.error("task_prompt_preparation_failed", session_id=session_id, run_id=run_id, kind=run.kind, error=str(e))
      return await self._record_launch_withheld(meta, run, f"prompt preparation failed: {e}")
    # Adapter entry to process start: context build, worktree preparation
    # and spawn all happen inside. An exception there is a launch failure
    # on an admitted run — the same durable-failure handler as backend
    # resolution.
    try:
      if meta.profile == "manager" and run.kind == "manager_turn":
        await self._execute_manager_turn(meta, run, option, snapshot)
      elif meta.profile == "worker" and run.kind in task_prompts.WORKER_KINDS:
        await self._execute_worker_run(meta, run, option, snapshot, launch_prompt=launch_prompt)
      else:
        raise task_errors.TaskInvalidError(
            f"run {run_id} (profile={meta.profile}, kind={run.kind}) has no executable adapter")
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
    for path in (self._cfg.charliebot_home, pathlib.Path(self._cfg.paths.worktree_dir)):
      free_gib = free_disk_gib(path)
      if free_gib < min_gib:
        return f"disk free {free_gib:.1f} GiB, below {min_gib} GiB ({path})"
    return None

  async def _record_launch_withheld(self, meta: models.SessionMetadata, run: models.RunRecord, reason: str) -> str:
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
    event_id = control_events.stable_withheld_event_id(run_id, reason)
    epoch = await tree.session_events.prime_aggregator(session_id)
    parent_epoch = (await tree.session_events.prime_aggregator(meta.task_parent_id) if meta.task_parent_id else None)
    async with tree.control_lock:
      events = tree.fact_history(session_id)
      existing = next((e for e in events if e.get("id") == event_id), None)
      if existing is not None:
        event = existing
      else:
        event = control_events.build_control_event(
            ET.RUN_LAUNCH_WITHHELD,
            actor=control_events.ACTOR_SYSTEM,
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
            actor=control_events.ACTOR_SYSTEM,
        )
    await tree.session_events.announce_appended_event(session_id, event, epoch=epoch)
    if report_event is not None and report_created and parent_epoch is not None:
      await tree.session_events.announce_appended_event(meta.task_parent_id, report_event, epoch=parent_epoch)
    if report_event is not None and report_created and meta.task_parent_id:
      await tree.dispatch.wake_parent(meta.task_parent_id, report=report_event)
    if meta.profile == "manager":
      await self._record_turn_launch_notice(session_id, run_id, reason)
    return f"withheld: {reason}"

  async def _land_launch_failure(self, meta: models.SessionMetadata, run: models.RunRecord, exc: Exception) -> None:
    """Land one post-admission launch exception as the Run's durable failure.

        The admitted run's evidence trail: the error text rides the run's
        events log (where the worker summary card's Events link and the
        failure-summary reader already look), the terminal ``run_finished`` fact carries
        outcome ``failed``, and the failed run then walks the same after-run
        path a failed process takes — the worker's failure report to its
        parent and the node's pending-input dispatch.
        Withheld and refused launch verdicts never reach this handler: they
        keep their no-terminal-fact semantics. The exception itself still
        propagates to the launch task's logging owner with its traceback.

        A launched run (it has a pid) that failed out of space is the one
        exception to the immediate landing: only the error evidence is
        written, the run stays unfinished, and the node reconcile pass drains
        it from its raw log once space returns — landing a failed end record
        here could contradict a success fact the delivery stage has yet to
        read. Any other error keeps the immediate landing, whose delivery
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
        await self._record_launch_error_event(session_id, run_id, error_text, quota_exhausted=None)
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
      await self._record_launch_error_event(session_id, run_id, error_text, quota_exhausted=None)
      await self._tree.dispatch.finish_run(session_id, run_id, outcome="failed", exit_code=-1)
      if meta.profile == "manager":
        await self._record_turn_launch_notice(session_id, run_id, error_text)
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
        # same as after a restart.
        self.start_run_end_landing_retry(session_id, run_id)

  async def _record_launch_error_event(
      self, session_id: str, run_id: str, error_text: str, *, quota_exhausted: bool | None) -> None:
    """Write the launch error into the run's events log as durable evidence.

        The events log is what the worker summary card's Events link opens and
        where ``_worker_failure_summary`` looks for the run's closing words, so
        the error is reachable without any new evidence channel.
        ``quota_exhausted`` is the ``LaunchRefused`` flag of a refused launch and
        None for every other launch failure: only a refusal's event carries the key.
        """

    events_log = self._tree.runs.run_dir(session_id, run_id) / runs.RUN_EVENTS_NAME
    event: dict = {
        "type": ET.ERROR,
        "message": error_text,
        "content": error_text,
        "timestamp": models.utc_now_iso(),
    }
    if quota_exhausted is not None:
      event[ET.QUOTA_EXHAUSTED] = quota_exhausted
    await ndjson.append_ndjson(events_log, event)
    # The worker summary card's Events link reads the record's events_ref: the
    # launch-failed run's evidence is reachable exactly the way a process
    # run's is (record_observation writes only the provided fields).
    await self._tree.runs.record_observation(session_id, run_id, events_ref=str(events_log))

  async def _record_turn_launch_notice(self, session_id: str, run_id: str, reason: str) -> None:
    """Persist and announce the chat error that reports a manager turn's launch failure or withhold.

        The event id derives from the Run and the reason, so a repeated launch
        failure or withhold of the same Run finds the event in the node's chat
        events and writes nothing. The content follows the Run's fresh record: a
        Run without a process id never started, one with a process id failed
        after its process started.
        """
    tree = self._tree
    run = await tree.runs.get_run(session_id, run_id)
    if run is None:
      raise runs.RunNotFoundError(runs.run_not_found_in_task_text(run_id, session_id))
    outcome = "Turn failed" if run.pid is not None else "Turn did not start"
    event_id = str(uuid.uuid5(control_events.TASK_ID_NAMESPACE, f"turn-launch:{run_id}:{reason}"))
    epoch = await tree.session_events.prime_aggregator(session_id)
    async with tree.control_lock:
      if any(e.get("id") == event_id for e in tree.fact_history(session_id)):
        return
      event = {
          "id": event_id,
          "type": ET.ERROR,
          "source": "turn_launch",
          "run_id": run_id,
          "content": f"{outcome}: {reason}",
          "timestamp": models.utc_now_iso(),
      }
      await tree.events.append(session_id, event)
    await tree.session_events.announce_appended_event(session_id, event, epoch=epoch)

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
      raise runs.RunNotFoundError(runs.run_not_found_in_task_text(run_id, session_id))
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

  def _resolve_run_backend(self, run: models.RunRecord) -> models.BackendOption:
    """The Run's explicitly recorded backend/model, resolved strictly."""
    if not run.backend:
      raise ValueError(f"run {run.id} records no backend; refusing to substitute one")
    return spawner_backends.resolve_backend_option(self._cfg, run.backend, run.model)

  def _child_env(self, session_id: str, run_id: str, agent_name: str) -> dict[str, str]:
    """The child CLI environment: its own identity, home and address.

        The child's session id and its own signed run token travel explicitly —
        never the parent's inherited identity — and CHARLIEBOT_HOME pins the
        selected home so the child CLI resolves this instance's config,
        credentials and server address.
        """
    key = config.configured_access_key()
    if not key:
      raise RuntimeError(
          "run-token signing requires credentials.yaml charliebot.access_key; "
          "agent child environments cannot be built without it")
    token = run_token.sign_run_token(
        run_token.RunTokenClaims(session_id=session_id, run_id=run_id, agent=agent_name or "worker"), key)
    return {
        constants.SESSION_ID_ENV_VAR: session_id,
        constants.RUN_TOKEN_ENV: token,
        "CHARLIEBOT_HOME": str(self._cfg.charliebot_home),
    }

  # ------------------------------------------------------------------
  # Manager turns
  # ------------------------------------------------------------------

  async def _alert_overlay_inactive(
      self,
      session_id: str,
      option: models.BackendOption,
      overlay_error: OSError | None,
  ) -> None:
    """The unified fenceless-run alert (undeclared or unreadable overlay)."""
    reason = "unreadable" if overlay_error is not None else "undeclared"
    log.warning("task_overlay_inactive", session_id=session_id, backend=option.id, reason=reason)
    callbacks = master_cc_queue.session_callbacks(self._events, self._lifecycle, self._anchors, self._sidebar)
    await callbacks.persist_and_broadcast(
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
      meta: models.SessionMetadata,
      run: models.RunRecord,
      option: models.BackendOption,
  ) -> task_prompts.PromptSnapshot:
    """Build, recheck, and durably commit this launch's instruction snapshot.

        The committed bytes are the bytes the adapters launch — never a second
        build. A declared-but-unreadable or undeclared overlay emits the unified
        fenceless-run alert through the shared run alert path.
        """
    snapshot, overlay_error, declared = await assemble_coherent_snapshot(self._cfg, self._tree, meta, run.kind, option)
    path = self._tree.runs.run_dir(meta.id, run.id) / task_prompts.SNAPSHOT_FILENAME
    await asyncio.to_thread(
        json_utils.atomic_write_text, path, json.dumps(snapshot.to_json_dict(), indent=2, ensure_ascii=False))
    await self._tree.runs.record_observation(meta.id, run.id, prompt_snapshot_ref=str(path))
    if not declared or overlay_error is not None:
      await self._alert_overlay_inactive(meta.id, option, overlay_error)
    return snapshot

  def _manager_finish_recorder(
      self, meta: models.SessionMetadata, run_id: str, option: models.BackendOption, transport_dir: pathlib.Path, *,
      ended_at: datetime.datetime | None) -> Callable[[str | None, int, dict], Awaitable[None]]:
    """The finish recorder both manager-turn launch paths hand the master queue.

    Records the run observation (native session id, served model, raw-log refs),
    lands the terminal fact through ``dispatch.finish_run``, and re-drives the
    node's pending inputs. ``ended_at=None`` keeps record_finish's
    observed-write-time default; the resume follow passes the drain rule's end
    time.
    """
    session_id = meta.id
    dispatcher = self._tree.dispatch

    async def on_task_finish(cc_session_id: str | None, exit_code: int, finish_extras: dict) -> None:
      try:
        await midturn_delivery.claim_finish_inputs(meta, dispatcher, run_id, transport_dir, successful=exit_code == 0)
        await self._tree.runs.record_observation(
            session_id,
            run_id,
            native_session_id=cc_session_id,
            model=finish_extras.get("model") or option.model,
            raw_log_ref=str(transport_dir / runs.RAW_LOG_NAME),
            result_ref=str(transport_dir / runs.RAW_LOG_NAME),
        )
        await dispatcher.finish_run(
            session_id,
            run_id,
            outcome="success" if exit_code == 0 else "failed",
            exit_code=exit_code,
            ended_at=ended_at,
        )
      finally:
        midturn_delivery.release_run(run_id)
      # Inputs admitted during this turn waited for the serialized
      # consumer; the turn's finish is what dispatches their next run.
      await dispatcher.dispatch_pending(session_id)

    return on_task_finish

  async def _execute_manager_turn(
      self,
      meta: models.SessionMetadata,
      run: models.RunRecord,
      option: models.BackendOption,
      snapshot: task_prompts.PromptSnapshot,
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

    session_id, run_id = meta.id, run.id
    transport_dir = self._tree.runs.run_dir(session_id, run_id)
    batch_ids = set(run.input_event_ids)
    batch_events = [e for e in self._tree.fact_history(session_id) if str(e.get("id")) in batch_ids]
    content, uploaded_files = task_input_prompt.compose_input_prompt(
        meta, batch_events, await self._tree.dispatch.child_report_provenances(batch_events))
    if not content:
      raise task_errors.TaskInvalidError(f"run {run_id} claimed no consumable input; nothing to execute")

    # The dispatched wake carries the scheduled session's duties (weekly
    # recycle, firing-report prefix) here. The recycle may clear the node's anchor in
    # place, so the fresh-conversation judgment below reads the post-recycle state.
    sequence_prefix = await apply_sequence_wake_duties(meta, batch_events)
    if sequence_prefix:
      content = f"{sequence_prefix}{content}"

    anchor_continues = _native_continuation_matches(
        meta.cc_session_id,
        meta.native_prompt_hash,
        meta.native_backend,
        meta.native_model,
        prompt_hash=snapshot.prompt_hash,
        option=option)
    fresh_native = not anchor_continues
    prompt = content
    if fresh_native and meta.cc_session_id is not None:
      # A reset, not a first turn: name the task and where the earlier
      # history lives, so the fresh native context can catch up without
      # the old conversation being copied or destroyed. A backend change
      # names the switch; every other change keeps the standing reason.
      if meta.native_backend and meta.native_backend != option.id:
        reason = session_anchors.backend_switch_reset_reason(meta.native_backend, option.id)
      else:
        reason = (
            "this task's managed instructions or sources changed since the "
            "previous turn, so this turn starts a fresh native conversation")
      goal = meta.task.goal if meta.task is not None else meta.name
      prompt = f"{session_anchors.context_reset_note(reason, goal)}\n\n{content}"

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

    on_task_finish = self._manager_finish_recorder(meta, run_id, option, transport_dir, ended_at=None)

    await self._persist_launch_text(session_id, run_id, prompt)
    log.info(
        "manager_turn_launching",
        session_id=session_id,
        run_id=run_id,
        backend=option.id,
        inputs=len(run.input_event_ids),
        prompt_hash=snapshot.prompt_hash[:12],
        fresh_native=fresh_native)
    await master_cc_queue.run_message(
        self._cfg,
        meta,
        prompt,
        master_cc_queue.session_callbacks(self._events, self._lifecycle, self._anchors, self._sidebar),
        user_event_ids=list(run.input_event_ids),
        auto_trigger=any(e.get("type") == ET.SCHEDULED_TRIGGER for e in batch_events),
        backend_option=option,
        uploaded_files=uploaded_files or None,
        task_instructions=snapshot.instructions_text,
        task_run=master_cc_state.TaskRunBinding(
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
      meta: models.SessionMetadata,
      run: models.RunRecord,
      option: models.BackendOption,
      snapshot: task_prompts.PromptSnapshot,
      *,
      launch_prompt: str | None,
  ) -> None:
    """One work, review, or sequence Run on the existing Worker/backend adapter.

        The context owner supplies every applicable managed rule/memory block
        exactly once — the committed ``snapshot`` rides the backend's
        system-instruction seam — while the controllers keep owning their task /
        step input: this adapter renders only the task/input context (bindings,
        pinned spec, claimed batch, sequence positions) from the same maintained
        template sections. A scheduled prompt override passes through the
        assembly.
        """
    session_id, run_id = meta.id, run.id
    run_dir = self._tree.runs.run_dir(session_id, run_id)
    events_log = run_dir / runs.RUN_EVENTS_NAME

    task_type = task_prompts.prompt_task_type(meta.task)
    continuation = self._run_continuation(meta, run, option, snapshot)
    continued = continuation is not None and continuation.native_id is not None
    review_worktree: str | None = None
    if run.kind == "review":
      work_run = await self._tree.runs.get_run(session_id, run.review_of_run_id or "")
      if work_run is None:
        raise task_errors.TaskInvalidError(f"review run {run_id} names no recorded work Run")
      # The review reuses the work Run's exact repo, branch and worktree.
      review_worktree = work_run.worktree_path
      if continued:
        context = await self._batch_only_context(meta, run)
      else:
        context = await self._build_review_context(session_id, work_run)
    elif run.sequence_ref is not None:
      # The owning controller composes a sequence Run's context: an iteration
      # returns its rendered bindings + loop position; a controller whose
      # answer is None (the scheduled step) keeps the runtime's step rendering
      # over the controller's verbatim launch prompt.
      controller = sequence_controllers.controller_for_sequence(run.sequence_ref)
      if controller is None:
        raise task_errors.TaskInvalidError(
            f"sequence run {run_id} carries owner_ref {run.sequence_ref.owner_ref!r} "
            "and no registered controller owns its kind")
      context = await controller.launch_context(meta, run, launch_prompt or "", self._cfg)
      if context is None:
        context = await self._build_step_context(meta, run, launch_prompt, task_type)
    elif launch_prompt is not None:
      context = await self._build_step_context(meta, run, launch_prompt, task_type)
    elif continued:
      context = await self._batch_only_context(meta, run)
    else:
      context = await self._build_work_context(meta, run, task_type)
    prompt = context
    if continuation is not None and continuation.native_id is None:
      # Trade-off 1: the backend, model or instructions moved while the node
      # waited, so the old conversation cannot continue. The fresh one opens
      # with the reset note — the branch and worktree are inherited either
      # way, and the previous Run's log carries what it already did.
      previous = continuation.previous
      if previous.backend and previous.backend != option.id:
        reason = session_anchors.backend_switch_reset_reason(previous.backend, option.id)
      elif previous.native_session_id is None:
        reason = NO_NATIVE_ID_RESET_REASON
      else:
        reason = (
            "this task's managed instructions or sources changed since the previous "
            "run, so this run starts a fresh native conversation")
      previous_log = self._tree.runs.run_dir(session_id, previous.id) / runs.RUN_EVENTS_NAME
      prompt = f"{worker_continuation_reset_note(reason, previous_log)}\n\n{context}"
    await self._persist_launch_text(session_id, run_id, prompt)

    binding = run_worker_binding(run_id, session_id)
    if backend_types.traits_for(option.type).preassigned_session_id:
      # A continued conversation keeps its own native id (the resume flag
      # names it); a fresh one gets the runtime-chosen id.
      preassigned = continuation.native_id if continued else str(uuid.uuid4())
      backend_types.lifecycle_for(option).assign_thread_native_id(binding, preassigned)

    async def on_spawned(spawned: models.ThreadMetadata) -> None:
      if spawned.pid is None or spawned.pid_start is None:
        raise RuntimeError(f"run {run_id} spawned without a pinned process identity")
      await self._tree.runs.record_launch(session_id, run_id, pid=spawned.pid, pid_start=spawned.pid_start)

    error = ""
    exit_code = -1
    worker_adapter: worker.Worker | None = None
    try:
      # A repository run starts in its worktree root: a review in its work Run's worktree, a work
      # Run in the worktree its context build recorded on the Run (read again here). A repo-less
      # run starts in its Run directory.
      bound = await self._tree.runs.get_run(session_id, run_id)
      assert bound is not None, f"run {run_id} lost its record while its launch context was built"
      worktree = review_worktree or bound.worktree_path
      working_dir = pathlib.Path(worktree) if worktree else run_dir
      # The Worker places the launch through the backend's lifecycle and relays the run when
      # the lifecycle asks for it (src/runtime/launch_loop.py). A refused launch — before the
      # first process or at a relay — ends the run through the LaunchRefused branch below.
      worker_adapter = worker.Worker(
          binding,
          working_dir,
          events_log,
          prompt,
          self._cfg,
          backend_option=option,
          on_spawned=on_spawned,
          extra_env=self._child_env(session_id, run_id, meta.name),
          instructions_content=snapshot.instructions_text,
          session_meta=meta,
          held_native_id=continuation.native_id if continued else None,
          run_kind=run.kind,
      )
      # Session-level notices (a pool login that needs re-login, a relay compaction) reach
      # the session chat through the successor chain.
      worker_adapter.on_session_event = functools.partial(self._successor.deliver_to_successor, session_id)
      exit_code = await worker_adapter.run()
    except backend_lifecycle.LaunchRefused as exc:
      if worker_adapter is not None:
        await worker_adapter.terminate()
      error = str(exc)
      if exc.quota_exhausted:
        self._quota_failures[(session_id, run_id)] = exc.resets_at
      log.warning(
          "task_run_launch_refused",
          session_id=session_id,
          run_id=run_id,
          error=error,
          quota_exhausted=exc.quota_exhausted)
      # The run's own events log carries the evidence: the failure-summary
      # reader and the sequence quota classification read this event.
      await self._record_launch_error_event(session_id, run_id, error, quota_exhausted=exc.quota_exhausted)
    except worker.QuotaExhaustedError as exc:
      if worker_adapter is not None:
        await worker_adapter.terminate()
      error = str(exc)
      self._quota_failures[(session_id, run_id)] = exc.resets_at
      log.warning("task_run_quota_exhausted", session_id=session_id, run_id=run_id, error=error)
    except Exception as exc:  # setup/transport failure: the run failed loudly
      log.error("task_run_failed", session_id=session_id, run_id=run_id, error=str(exc), exc_info=True)
      if worker_adapter is not None:
        await worker_adapter.terminate()
      error = str(exc)

    durable_outcome = await self._finalize_worker_run(meta, run, option, exit_code=exit_code, error=error)
    # The launch's own writes (worktree facts, native session id) landed on
    # the DURABLE record after this in-memory snapshot was taken; the
    # delivery chain must judge the recorded provenance, not the stale one.
    fresh = await self._tree.runs.get_run(session_id, run_id)
    if fresh is not None:
      run = fresh
    await self._after_worker_run(meta, run, durable_outcome)

  def _previous_run(self, session_id: str, run_id: str) -> models.RunRecord | None:
    """The node's latest Run record before *run_id* (the canonical launch order), or None."""
    previous: models.RunRecord | None = None
    for record in self._tree.runs.list_run_records_sync(session_id):
      if record.id != run_id:
        previous = record
    return previous

  def _run_continuation(
      self,
      meta: models.SessionMetadata,
      run: models.RunRecord,
      option: models.BackendOption,
      snapshot: task_prompts.PromptSnapshot,
  ) -> RunContinuation | None:
    """The continuation this launch performs, or None for an independent launch.

        A dispatcher-created run on a worker node whose delivery episode is
        still open continues the node's latest Run — its registration already
        inherited the repo, base, branch and worktree. The native conversation
        continues when the backend identity, model and prompt hash all match
        the previous Run's recorded launch (the manager's continuation rule);
        otherwise the launch keeps the inheritance but starts a fresh
        conversation. Retried Runs and sequence Runs keep their own launch
        shape, and a review Run is a continuation only where the delivery
        chain could not have spawned it: right after a successful review of
        the same work Run whose merge paused.
        """
    if run.sequence_ref is not None or run.retry_of_run_id is not None:
      return None
    if meta.task is not None and meta.task.task_type == models.TaskType.COLD_READ:
      # Every cold-read run opens a new conversation: the reader answers from
      # the page alone, with no memory of a previous attempt.
      return None
    previous = self._previous_run(meta.id, run.id)
    if previous is None:
      return None
    if run.kind == "review":
      events = self._tree.runs.load_events_sync(meta.id)
      if (previous.kind != "review" or previous.review_of_run_id != run.review_of_run_id or
          self._tree.runs.terminal_outcome(events, previous.id) != "success"):
        return None
    elif run.kind == "work":
      # An episode that closed (delivered or cancelled) starts fresh.
      if self._tree.facts_of(meta.id).close_events:
        return None
    else:
      return None
    native_id: str | None = None
    if _native_continuation_matches(previous.native_session_id, _snapshot_prompt_hash(previous), previous.backend,
                                    previous.model, prompt_hash=snapshot.prompt_hash, option=option):
      native_id = previous.native_session_id
    return RunContinuation(previous=previous, native_id=native_id)

  async def _claimed_batch_content(self, meta: models.SessionMetadata, run: models.RunRecord) -> str:
    """The input-prompt content of the events this run claims as its batch."""
    batch_ids = set(run.input_event_ids)
    batch = [e for e in self._tree.fact_history(meta.id) if str(e.get("id")) in batch_ids]
    provenance = await self._tree.dispatch.child_report_provenances(batch) if batch else {}
    content, _uploads = task_input_prompt.compose_input_prompt(meta, batch, provenance) if batch else ("", [])
    return content

  async def _batch_only_context(self, meta: models.SessionMetadata, run: models.RunRecord) -> str:
    """The continued conversation's whole task/input context: this run's claimed batch.

        The conversation already holds the task spec and the worktree
        bindings from the earlier run, so a continued run's prompt is only
        what is new. An empty batch is impossible — the dispatcher claims one
        before this launch exists — and surfaces loudly.
        """
    content = await self._claimed_batch_content(meta, run)
    if not content.strip():
      raise task_errors.TaskInvalidError(f"run {run.id} continues a conversation but claims no consumable input")
    return content

  async def _finalize_worker_run(
      self,
      meta: models.SessionMetadata,
      run: models.RunRecord,
      option: models.BackendOption,
      *,
      exit_code: int,
      error: str,
      ended_at: datetime.datetime | None = None,
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
        events_ref=str(run_dir / runs.RUN_EVENTS_NAME),
        result_ref=str(raw_path),
    )
    await self._tree.dispatch.finish_run(
        session_id, run_id, outcome=outcome, exit_code=exit_code if not error else -1, ended_at=ended_at)
    durable = await self._tree.runs.terminal_outcome_of(session_id, run_id) or outcome
    if error:
      log.warning("task_run_error", session_id=session_id, run_id=run_id, error=error[:500])
    return durable

  async def _worker_outcome(self, run_dir: pathlib.Path, option: models.BackendOption) -> str:
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
      _events, result, _code = await asyncio.to_thread(runs.scan_result_exit, raw_path, translate)
      if result is not None and runs.result_success(result):
        return "success"
    events_log = run_dir / runs.RUN_EVENTS_NAME
    if events_log.is_file():
      _found, success = await asyncio.to_thread(self._events_log_result_success, events_log)
      if success:
        return "success"
    return "failed"

  @staticmethod
  def _events_log_result_success(events_log: pathlib.Path) -> tuple[bool, bool]:
    """(found, success) of the last RESULT event in a translated events log."""
    found = False
    success = False
    for event in runs.parse_raw_lines(events_log.read_bytes()):
      if event.get("type") == ET.RESULT:
        found = True
        success = runs.result_success(event)
    return found, success

  async def _native_session_id(self, raw_path: pathlib.Path, option: models.BackendOption) -> str | None:
    """The backend's native session id from the run's raw stream, when one exists."""
    if not raw_path.is_file():
      return None
    translate = self._fresh_translate(option)

    def _scan() -> str | None:
      for event in runs.parse_raw_lines(raw_path.read_bytes()):
        # Claude's translate passes its system/init event through unchanged, so
        # its conversation id arrives as the raw event, not as a session_attached one.
        if event.get("type") == "system" and event.get("subtype") == "init" and event.get("session_id"):
          return str(event["session_id"])
        for translated in translate(event):
          if translated.get("type") == ET.SESSION_ATTACHED and translated.get("session_id"):
            return str(translated["session_id"])
      return None

    return await asyncio.to_thread(_scan)

  def _fresh_translate(self, option: models.BackendOption) -> Callable[[dict], list[dict]]:
    """A fresh translate callable for one whole-file scan (stateful translates need one instance)."""
    try:
      return backend_types.build_backend(option, self._cfg).translate_event
    except Exception as e:
      # Translate-only construction may lack the CLI binary; the scan
      # degrades to the raw claude shape the same way restart recovery's
      # translate fallback does — never a crash of the finalize path.
      log.warning("task_run_translate_unresolved", backend=option.id, error=str(e))
      return lambda event: [event]

  # ------------------------------------------------------------------
  # Prompts (existing sources; the launch text rides the Run as evidence)
  # ------------------------------------------------------------------

  async def _work_description(self, meta: models.SessionMetadata, run: models.RunRecord) -> str:
    """The task/input body: the pinned spec's goal plus this run's claimed batch."""
    task = meta.task
    spec_text = task.goal if (task is not None and task.goal.strip()) else ""
    content = await self._claimed_batch_content(meta, run)
    description = "\n\n".join(part for part in (spec_text, content) if part)
    if not description.strip():
      raise task_errors.TaskInvalidError(f"run {run.id} has no task spec and no input; nothing to execute")
    return description

  async def _build_work_context(
      self, meta: models.SessionMetadata, run: models.RunRecord, task_type: models.TaskType) -> str:
    """The work Run's task/input context from the maintained template sections.

        Verify runs stay read-only: their contract is entirely managed
        instructions, so their context is the pinned spec and batch alone (no
        worktree, no review artifacts). Repo-less work runs keep the run dir as
        the working directory. A cold-read run's context is the page text and
        the eight questions alone (no session info, no task-body wrapper).
        """
    if task_type == models.TaskType.COLD_READ:
      return await self._build_cold_read_context(meta, run)
    description = await self._work_description(meta, run)
    task = meta.task
    if task_type == models.TaskType.VERIFY:
      return task_prompts.render_task_body(self._cfg, description)
    parts = [task_prompts.render_session_info(self._cfg, meta.name)]
    if not (task is not None and task.repo_path):
      parts.append(task_prompts.render_task_body(self._cfg, description))
      return "\n\n".join(part for part in parts if part)
    assert task.repo_path is not None
    repo_path = pathlib.Path(task.repo_path).resolve()
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
            intro_line=task_prompts.binding_intro(self._cfg, run),
            branch_name=branch_name,
            base_branch_origin=origin,
            wt_path=worktree_path,
            repo_path=str(repo_path)))
    parts.append(task_prompts.render_task_body(self._cfg, description))
    if task is not None and task.keep_worktree:
      parts.append(task_prompts.render_worktree_persistence(self._cfg))
    return "\n\n".join(part for part in parts if part)

  async def _build_cold_read_context(self, meta: models.SessionMetadata, run: models.RunRecord) -> str:
    """The cold-read run's whole input: the page text, one blank line, the eight questions.

        The reader answers from the page alone, so the context carries no
        session info and no task-body wrapper. The launch reads the page and
        records the bytes' sha256 on the run's evidence file: the completion
        report states what the cold read read, even when the page moved since.
        """
    cold = meta.task.cold_read if meta.task is not None else None
    assert cold is not None, f"cold-read run {run.id} of task {meta.id} carries no cold_read spec"
    page_bytes = await asyncio.to_thread(pathlib.Path(cold.page_path).read_bytes)
    evidence = {"page_path": cold.page_path, "page_sha256": hashlib.sha256(page_bytes).hexdigest()}
    evidence_path = self._tree.runs.run_dir(meta.id, run.id) / cold_read.EVIDENCE_FILENAME
    await asyncio.to_thread(json_utils.atomic_write_text, evidence_path, json.dumps(evidence))
    _system_prompt, questions = cold_read.load_texts(self._cfg.charlie_bot_repo)
    return cold_read.probe_input(page_bytes.decode("utf-8"), questions, cold.trigger)

  async def _build_step_context(
      self,
      meta: models.SessionMetadata,
      run: models.RunRecord,
      step_prompt: str,
      task_type: models.TaskType,
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
              intro_line=task_prompts.binding_intro(self._cfg, run),
              branch_name=run.branch_name,
              base_branch_origin=f"`{run.base_branch}`",
              wt_path=run.worktree_path,
              repo_path=run.repo_path))
    parts.append(task_prompts.render_task_body(self._cfg, step_prompt))
    return "\n\n".join(part for part in parts if part)

  async def _later_messages_section(self, meta: models.SessionMetadata, work_run: models.RunRecord) -> str:
    """The review's Later Messages section: every input the node's work runs claimed, first run
    through *work_run*, rendered in claim order — or "" when they claimed none.

        A message a work run claimed reached the worker as its prompt, so the
        review judges the work against the same input the worker received: the
        task spec, then each claimed message (agent messages and
        scheduled-trigger wakes alike). ``compose_input_prompt`` renders them,
        the function the work Run's own prompt used; the section keeps the
        runs' order and each run's own list order. No claimed input, no
        section: the task spec then sits directly before the worker summary.
        """
    claimed_ids: list[str] = []
    for run in self._tree.runs.list_run_records_sync(meta.id):
      if run.kind != "work":
        continue
      claimed_ids.extend(run.input_event_ids)
      if run.id == work_run.id:
        break
    else:
      raise task_errors.TaskInvalidError(f"review of run {work_run.id}: it is not a work run of node {meta.id}")
    if not claimed_ids:
      return ""
    events_by_id = {str(event.get("id")): event for event in self._tree.fact_history(meta.id)}
    events = [events_by_id[event_id] for event_id in claimed_ids]
    provenance = await self._tree.dispatch.child_report_provenances(events)
    content, _uploads = task_input_prompt.compose_input_prompt(meta, events, provenance)
    if not content.strip():
      return ""
    return f"**Later Messages:**\n{content}"

  async def _review_inputs(
      self, session_id: str,
      work_run: models.RunRecord) -> tuple[models.SessionMetadata, str | None, pathlib.Path, pathlib.Path]:
    """The task record, work report and two log paths every review context reads.

        The task record carries the spec the review judges against. ``extract_review_context`` reads the work
        report from the work Run's events log; the chat log is the node's, and the same events log goes to the
        rendered context.
        """
    meta = await self._tree.load_meta(session_id)
    assert meta is not None and meta.task is not None, (
        f"review of work run {work_run.id} needs its node's task record for the spec")
    worker_log_path = self._tree.runs.run_dir(session_id, work_run.id) / runs.RUN_EVENTS_NAME
    worker_summary = await review.extract_review_context(session_id, work_run.id, worker_log_path=worker_log_path)
    chat_log_path = chat_events.chat_events_path(self._cfg.sessions_dir / session_id)
    return meta, worker_summary, chat_log_path, worker_log_path

  async def _build_review_context(self, session_id: str, work_run: models.RunRecord) -> str:
    """The review Run's task/input context: the work being judged and its git steps.

        The reviewer's stable contract rides the managed instructions
        (review_rules_text); this context names the exact work Run, its logs,
        and the volatile git steps — it never turns the reviewer into an
        implementer beyond the checklist's minimal-fix rule. The task spec is
        followed by the Later Messages section when the node's work runs
        claimed input after it. A repo-less work Run reviews paths instead of
        a diff.
        """
    if not work_run.repo_path:
      return await self._build_repo_less_review_context(session_id, work_run)
    assert (work_run.branch_name and work_run.worktree_path and work_run.repo_path and work_run.base_branch), (
        f"review of work run {work_run.id} needs its exact repo/base/branch/worktree "
        "provenance; an unset base is never silently replaced with main")
    meta, worker_summary, chat_log_path, worker_log_path = await self._review_inputs(session_id, work_run)
    context_lines = review.review_context_lines(
        review.review_spec_text(meta.task), await self._later_messages_section(meta, work_run), worker_summary,
        f"(work run {work_run.id})")
    return task_prompts.review_task_context(
        branch_name=work_run.branch_name,
        wt_path=work_run.worktree_path,
        base_branch=work_run.base_branch,
        chat_log_path=chat_log_path,
        worker_log_path=worker_log_path,
        context_section="\n".join(context_lines),
    )

  async def _build_repo_less_review_context(self, session_id: str, work_run: models.RunRecord) -> str:
    """The repo-less review's task/input context: spec and reported paths, no git.

        Without a repository there is no diff to read and nothing to merge:
        the review context is the task spec (its acceptance tests included),
        the Later Messages section when the node's work runs claimed input
        after it, plus the work report's path list, and the reviewer checks
        the current state of those paths against the acceptance tests. The
        spec comes from the task record, the same record the work Run's
        prompt reads.
        """
    meta, worker_summary, chat_log_path, worker_log_path = await self._review_inputs(session_id, work_run)
    return task_prompts.repo_less_review_task_context(
        chat_log_path=chat_log_path,
        worker_log_path=worker_log_path,
        spec_text=review.review_spec_text(meta.task),
        later_messages=await self._later_messages_section(meta, work_run),
        work_report=worker_summary,
    )

  async def _persist_launch_text(self, session_id: str, run_id: str, prompt: str) -> None:
    """Retain the launch's exact task/input text on the Run as separate evidence.

        The managed instruction half lives in the Run's committed snapshot
        (``prompt_snapshot.json`` via ``prompt_snapshot_ref``); this file keeps
        the volatile half — bindings, pinned spec, claimed batch, sequence
        positions — exactly as it was handed to the adapter.
        """
    path = self._tree.runs.run_dir(session_id, run_id) / task_prompts.LAUNCH_TEXT_FILENAME
    json_utils.atomic_write_text(path, prompt)

  async def _prepare_worktree(self, meta: models.SessionMetadata, run: models.RunRecord,
                              repo_path: pathlib.Path) -> tuple[str, str, str, str | None]:
    """Create the Run's isolated worktree from the requested base; records it on the Run.

        An unattended launch starts from the remote's published default branch
        when no base was requested; a requested base is used verbatim. The
        created branch, worktree and canonical base land on the Run record.
        """
    remote_tip: str | None = None
    if run.base_branch:
      base_branch = run.base_branch
    elif meta.task is not None and meta.task.base_branch:
      base_branch = meta.task.base_branch
    else:
      default_branch, remote_tip = await git.git_remote_default_branch_and_tip(repo_path)
      base_branch = f"origin/{default_branch}"
    branch_name = run.branch_name or f"charliebot/task-{int(time.time())}-{run.id[:8]}"
    wt_path = pathlib.Path(self._cfg.paths.worktree_dir) / git.git_worktree_dir_name(branch_name)
    pathlib.Path(self._cfg.paths.worktree_dir).mkdir(parents=True, exist_ok=True)
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
      raise runs.RunNotFoundError(runs.run_not_found_in_task_text(run_id, session_id))
    if run.pid is None or run.pid_start is None:
      raise task_errors.TaskInvalidError(f"run {run_id} records no launched process identity; nothing to re-attach")
    events = tree.runs.load_events_sync(session_id)
    if tree.runs.run_has_terminal_fact(run, events):
      return False
    if is_alive is None:
      # The re-evaluable probe, not a captured boolean: the follow must
      # observe the matching process ENDING (pid reuse and descendants
      # that kept stdout are judged by the existing identity rules on
      # every poll), and a stopped/ended process must converge to its
      # durable result instead of following forever.
      is_alive = runs.run_alive_probe(run.pid, run.pid_start, run.started_at, runs.read_host_boot_time())
    meta = await tree.load_meta(session_id)
    if meta is None:
      raise task_errors.task_not_found(session_id)
    alive_on_entry = is_alive()
    ended_at: datetime.datetime | None = None
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
    if meta.profile == "worker" and run.kind in task_prompts.WORKER_KINDS:
      await self._resume_worker_run(meta, run, option, is_alive, ended_at=ended_at)
      return False
    raise task_errors.TaskInvalidError(f"run {run_id} (kind={run.kind}) has no resume adapter")

  async def _resume_manager_turn(
      self,
      meta: models.SessionMetadata,
      run: models.RunRecord,
      option: models.BackendOption,
      is_alive: Callable[[], bool],
      *,
      ended_at: datetime.datetime | None,
  ) -> asyncio.Future:
    """Re-attach a v2 manager turn through the per-session queue's follow path.

        Returns the future the queue consumer resolves with the followed
        turn's result: the caller keeps it (never awaits it) so the follow's
        out-of-space failure can reach the end-landing retry after this call
        has returned.
        """

    transport_dir = self._tree.runs.run_dir(meta.id, run.id)
    record = models.MasterRunRecord(
        pid=run.pid,
        pid_start=run.pid_start,
        started_at=run.started_at,
        raw_log=str(transport_dir / runs.RAW_LOG_NAME),
    )

    async def on_task_spawn(pid: int, pid_start: str | None) -> None:
      raise RuntimeError(f"resume follow of run {run.id} must not spawn a process")

    on_task_finish = self._manager_finish_recorder(meta, run.id, option, transport_dir, ended_at=ended_at)

    return await master_cc_queue.enqueue_master_resume(
        self._cfg,
        meta,
        record,
        master_cc_queue.session_callbacks(self._events, self._lifecycle, self._anchors, self._sidebar),
        is_alive=is_alive,
        task_run=master_cc_state.TaskRunBinding(session_id=meta.id, run_id=run.id, transport_dir=str(transport_dir)),
        on_task_spawn=on_task_spawn,
        on_task_finish=on_task_finish,
        extra_env=self._child_env(meta.id, run.id, meta.name),
    )

  async def _resume_worker_run(
      self,
      meta: models.SessionMetadata,
      run: models.RunRecord,
      option: models.BackendOption,
      is_alive: Callable[[], bool],
      *,
      ended_at: datetime.datetime | None,
  ) -> None:
    """Re-attach a worker Run through Worker.resume's tail-follow.

        ``ended_at`` carries the drain rule's end time (the raw log's last
        write) for a process dead on entry; a live re-attach passes None and
        keeps the observed-exit write time.
        """
    session_id, run_id = meta.id, run.id
    run_dir = self._tree.runs.run_dir(session_id, run_id)
    events_log = run_dir / runs.RUN_EVENTS_NAME
    binding = run_worker_binding(run_id, session_id, pid=run.pid, pid_start=run.pid_start)
    if backend_types.traits_for(option.type).preassigned_session_id:
      backend_types.lifecycle_for(option).assign_thread_native_id(binding, run.native_session_id)
    worker_adapter = worker.Worker(
        binding,
        pathlib.Path(run.worktree_path) if run.worktree_path else run_dir,
        events_log,
        "",  # the follow builds nothing: the prompt text was the launch's
        self._cfg,
        backend_option=option,
        run_kind=run.kind,
    )
    # Session-level notices reach the session chat exactly as a fresh run's do.
    worker_adapter.on_session_event = functools.partial(self._successor.deliver_to_successor, session_id)
    exit_code = await worker_adapter.resume(is_alive=is_alive, on_silence=None)
    durable_outcome = await self._finalize_worker_run(
        meta, run, option, exit_code=exit_code, error="", ended_at=ended_at)
    await self._after_worker_run(meta, run, durable_outcome)

  async def repair_end_metadata(self, session_id: str, run: models.RunRecord, outcome: str) -> None:
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

  def take_quota_failure(self, session_id: str, run_id: str) -> tuple[bool, datetime.datetime | None]:
    """Pop the run's quota failure: (True, its reset moment or None) when it ended on quota exhaustion.

        The record lives in this process only. A run that ended otherwise yields (False, None).
        """
    if (session_id, run_id) not in self._quota_failures:
      return False, None
    return True, self._quota_failures.pop((session_id, run_id))

  def _pool_chose_backend(self, meta: models.SessionMetadata, run: models.RunRecord) -> bool:
    """True when the run's backend came from the shared rotation: a review run, a node whose
    delegation named none, or a node whose router route the pool fallback answered."""
    if run.kind == "review":
      return True
    fields = metadata_slots.fields_of(meta, backend_pool.METADATA_OWNER)
    return fields.backend_from_pool or bool(fields.route is not None and fields.route.backends)

  async def _fail_over_quota(self, meta: models.SessionMetadata, run: models.RunRecord) -> bool:
    """Continue a quota-interrupted run on the next pool member, in the same worktree; False when none is available.

        The member that ran out is already cooling. The node moves to the new member, so its later
        revision rounds and messages run there too. The retry binds the interrupted run's input batch:
        a revision round's retry carries the same review feedback.
        """
    session_id = meta.id
    route = metadata_slots.fields_of(meta, backend_pool.METADATA_OWNER).route
    member = None
    if route is not None and route.backends:
      member = backend_pool.pick_ordered(
          self._cfg, caller="failover", backends=route.backends, exclude={run.backend or ""})
    if member is None:
      member = backend_pool.select(self._cfg, caller="failover", exclude={run.backend or ""})
    if member is None:
      return False
    await self._tree.set_node_backend(session_id, member.id)
    retry = await self._tree.create_retry(
        session_id,
        f"quota-failover:{run.id}",
        run.id,
        backend=member.id,
        input_event_ids=list(run.input_event_ids),
    )
    until = backend_pool.cooling_until(run.backend or "")
    await self._events.persist_and_broadcast(
        session_id, {
            "type": ET.QUOTA_FAILOVER,
            "from": run.backend or "",
            "to": member.id,
            "recovers_at": until.isoformat(timespec="minutes") if until is not None else "",
        })
    log.warning(
        "worker_quota_failover",
        session_id=session_id,
        run_id=run.id,
        retry_run_id=retry["run_id"],
        from_backend=run.backend,
        to_backend=member.id,
    )
    self.launch(session_id, retry["run_id"])
    return True

  async def _after_worker_run(self, meta: models.SessionMetadata, run: models.RunRecord, durable_outcome: str) -> None:
    """The delivery chain one worker Run's durable outcome drives.

        A work Run finishing is not delivery: an implement task's review and
        target-branch landing must complete first, and a failed or blocked
        outcome reports the parent with durable stable evidence; the report's
        delivery archives the stopped worker (its worktree stays on disk —
        only a completed node removes it). A Run that a sequence controller
        launched has no delivery chain of its own: its controller owns
        progression and the one final report, so this method only asks it and
        re-enters the node's own dispatcher for inputs the Run left pending.
        """
    session_id = meta.id
    try:
      if run.sequence_ref is not None:
        controller = sequence_controllers.controller_for_sequence(run.sequence_ref)
        if controller is None:
          log.warning(
              "sequence_run_without_controller",
              session_id=session_id,
              run_id=run.id,
              owner_ref=run.sequence_ref.owner_ref)
          return
        await controller.after_run(session_id, run, self._tree, self._cfg)
        return
      quota_failed, resets_at = self.take_quota_failure(session_id, run.id)
      if quota_failed and run.backend and self._pool_chose_backend(meta, run):
        backend_pool.cool(run.backend, resets_at)
      if run.kind == "review":
        await self._after_review_run(meta, run, durable_outcome)
        return
      if (meta.task is not None and meta.task.task_type == models.TaskType.COLD_READ and
          durable_outcome in ("success", "failed")):
        await self._after_cold_read_run(meta, run, durable_outcome)
        return
      if durable_outcome == "success":
        task_type = meta.task.task_type if meta.task is not None else models.TaskType.IMPLEMENT
        if task_type == models.TaskType.IMPLEMENT:
          pause = self._tree.completion.pause_reason(session_id)
          if pause is not None:
            # A paused run is not a delivery: no review spawns, the task
            # stays open on its branch and worktree, and the wake's
            # continuation run re-enters this chain when it ends.
            log.info("worker_delivery_paused", session_id=session_id, run_id=run.id, reason=pause)
            return
          review_id = await self._maybe_spawn_review(session_id, run)
          if review_id is None:
            # The reviewer never runs on a member the worker node used, so a
            # pool without a free member spawns no review: report blocked with
            # each member's recovery time, as an exhausted retry chain does.
            await self._report_failure_to_parent(
                session_id, run, "blocked", summary=_no_reviewer_summary(run, self._cfg))
        else:
          await self._cleanup_worktree_if_delivered(session_id, run)
        return
      if durable_outcome in ("failed", "interrupted"):
        summary = None
        if quota_failed and durable_outcome == "failed" and self._fails_over(meta, run):
          if await self._fail_over_quota(meta, run):
            return
          summary = (
              f"{await self._worker_failure_summary(session_id, run)}\n\n{backend_pool.no_member_message(self._cfg)}")
        await self._report_failure_to_parent(session_id, run, durable_outcome, summary=summary)
    finally:
      # Inputs admitted during this run waited for the serialized
      # consumer; the finish chain (review queued/landed, closure,
      # failure report) ran first, so this dispatch only sees what that
      # chain left pending. The review path reaches it too, so the
      # inputs admitted during a review Run dispatch here as well.
      await self._tree.dispatch.dispatch_pending(session_id)

  def _fails_over(self, meta: models.SessionMetadata, run: models.RunRecord) -> bool:
    """True when a run's quota failure moves to the next pool member: a work run of a pool-chosen node."""
    if run.kind != "work" or meta.task is None:
      return False
    if meta.task.task_type not in (models.TaskType.IMPLEMENT, models.TaskType.QUICK_EDIT, models.TaskType.SCRIPT_RUN,
                                   models.TaskType.VERIFY):
      return False
    return self._pool_chose_backend(meta, run)

  async def _after_review_run(self, meta: models.SessionMetadata, run: models.RunRecord, durable_outcome: str) -> None:
    session_id = meta.id
    work_run = await self._tree.runs.get_run(session_id, run.review_of_run_id or "")
    if work_run is None:
      raise task_errors.TaskInvalidError(f"review run {run.id} names no recorded work Run")
    if durable_outcome == "failed":
      retried = await self._maybe_spawn_review(session_id, work_run)
      if retried is None:
        await self._report_failure_to_parent(
            session_id,
            work_run,
            "blocked",
            summary=f"review of work run {work_run.id} failed on every configured reviewer backend; "
            f"backends.pool: {backend_pool.recovery_report(self._cfg)}",
            deciding_run=run)
      elif self._tree.runs.terminal_outcome(self._tree.runs.load_events_sync(session_id), retried) == "success":
        # The chain ends at the first successful review, so no retry exists for a failed run that follows
        # one: it is the continuation of a paused review, and it reports like any failed run.
        await self._report_failure_to_parent(session_id, run, durable_outcome)
      return
    if durable_outcome == "interrupted":
      # An interrupted review reports like an interrupted work run; it never
      # retries, and its own end is the node's last.
      await self._report_failure_to_parent(session_id, run, durable_outcome)
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
    pause = self._tree.completion.pause_reason(session_id)
    if pause is not None:
      # A paused review is not a merge: the wake's continuation review
      # delivers when it ends with nothing pending.
      log.info("worker_delivery_paused", session_id=session_id, run_id=run.id, reason=pause)
      return
    refs = [f"{task_completion.RUN_REF_PREFIX}{work_run.id}"]
    if work_run.task_spec_hash:
      refs.append(f"{task_completion.SPEC_REF_PREFIX}{work_run.task_spec_hash}")
    refs.append(f"{task_completion.REVIEW_REF_PREFIX}{run.id}")
    if work_run.repo_path:
      # A repo task's implement delivery lands its reviewed branch on the
      # base; an unproven landing is a blocked report, not a close.
      landing, reason = await self._landing_for_work(work_run)
      if landing is None:
        # A review that stopped before its push states its verdict in its closing words; the report quotes them.
        events_log = self._tree.runs.run_dir(session_id, run.id) / runs.RUN_EVENTS_NAME
        closing_words = await asyncio.to_thread(review._worker_summary_from_events_log, events_log)
        verdict = closing_words or "The review run left no closing words, so its verdict is unknown."
        await self._report_failure_to_parent(
            session_id,
            work_run,
            "blocked",
            summary=f"review run {run.id} ended without landing work run {work_run.id} on "
            f"{work_run.base_branch or 'the requested base'}: {reason}\n\nReview verdict:\n{verdict}",
            deciding_run=run)
        return
      branch, commit, repo_path = landing
      refs.append(f"{task_completion.LANDING_REF_PREFIX}{branch}@{commit}")
      evidence = task_completion.CompletionEvidence(
          summary=f"work run {work_run.id} delivered after review {run.id} landed {commit[:12]} on {branch}",
          result_refs=refs,
          run_ids=[work_run.id],
          review_run_ids=[run.id],
          landing=task_completion.LandingEvidence(branch=branch, commit=commit, repo_path=repo_path),
      )
    else:
      # A repo-less task's implement delivery is the reviewer's verdict
      # on the reported paths; no landing exists to prove.
      evidence = task_completion.CompletionEvidence(
          summary=f"work run {work_run.id} delivered after review {run.id} "
          "checked the reported paths against the acceptance tests",
          result_refs=refs,
          run_ids=[work_run.id],
          review_run_ids=[run.id],
          landing=None,
      )
    try:
      await self._tree.completion.evaluate_automatic_completion(session_id, run_id=work_run.id, evidence=evidence)
    except task_errors.TaskConflictError as e:
      log.warning(
          "task_delivery_close_blocked",
          session_id=session_id,
          run_id=work_run.id,
          blockers=getattr(e, "blockers", None))
    finally:
      await self._cleanup_worktree_if_delivered(session_id, work_run)

  async def _maybe_spawn_review(self, session_id: str, work_run: models.RunRecord) -> str | None:
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
        reviewer retries down the backends.pool rotation with distinct Run
        records; exhausted retries keep the worktree (a repo task's) and
        report blocked.
        """
    tree = self._tree
    async with tree.control_lock:
      if tree.task_state(session_id) != "open":
        return None
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
      used = {r.backend for r in tree.runs.list_run_records_sync(session_id) if r.backend}
      tried = [r.backend for r in existing if r.backend]
      # A routed node's reviewer reuses the worker's route list (no router call): the first
      # available member that avoids the worker's and the node's used backends. An empty or fully
      # avoided list, and every unrouted node, take the pool selection.
      meta = await tree.load_meta(session_id)
      route = metadata_slots.fields_of(meta, backend_pool.METADATA_OWNER).route if meta else None
      selection = None
      if route is not None and route.backends:
        option = backend_pool.pick_ordered(
            self._cfg, caller="reviewer", backends=route.backends, exclude={work_run.backend or "", *used, *tried})
        if option is not None:
          selection = (option.id, models.option_default_model(option, subject="routed backend "), tried)
      if selection is None:
        selection = review.select_reviewer_backend(
            self._cfg, "reviewer", work_run.backend or "", work_run.model, tried, used_backends=used)
      if selection is None:
        log.warning("reviewer_backends_exhausted", session_id=session_id, work_run=work_run.id)
        return None
      resolved_backend, resolved_model, _tried = selection
      run_id = control_events.stable_run_id(session_id, f"review:{work_run.id}:{attempts + 1}")
      record = models.RunRecord(
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

  async def _landing_for_work(self, work_run: models.RunRecord) -> tuple[tuple[str, str, str] | None, str]:
    """The (branch, commit, repo) landing evidence of a reviewed work Run, or None and why.

        The commit is the work branch's tip in its repository; the check
        requires that commit to exist and be an ancestor of the branch the
        reviewer's push publishes (review.review_landing_target, fetched
        first), never the possibly stale local branch of the same name.
        """
    if not (work_run.repo_path and work_run.branch_name and work_run.base_branch):
      return None, f"work run {work_run.id} records no repo, branch and base to verify"
    commit = await git.git_rev_parse(pathlib.Path(work_run.repo_path), work_run.branch_name)
    if commit is None:
      return None, f"branch {work_run.branch_name} does not resolve in {work_run.repo_path}"
    target = review.review_landing_target(work_run.base_branch)
    landed, reason = await git.git_verify_commit_landed(pathlib.Path(work_run.repo_path), target, commit)
    if not landed:
      return None, reason
    return (target, commit, work_run.repo_path), ""

  async def _cleanup_worktree_if_delivered(self, session_id: str, work_run: models.RunRecord) -> None:
    """Remove the shared worktree only once the task actually delivered.

        Only a completed node loses its worktree: failed, blocked, unproven
        and archived outcomes all keep it, and keep_worktree pins it.
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
        pathlib.Path(self._cfg.paths.worktree_dir),
        log_fields={
            "run_id": work_run.id,
            "session": session_id
        },
        label="Task worktree",
        fail_event="task_worktree_cleanup_failed",
        remove_failed_event="task_worktree_remove_failed",
    )

  async def _after_cold_read_run(
      self, meta: models.SessionMetadata, run: models.RunRecord, durable_outcome: str) -> None:
    """The cold-read delivery chain: report the answer, retry the next backend, or report every model failed.

        A cold-read run succeeds only when its final answer text is not empty.
        A failed run — or a successful run whose answer is empty — records its
        backend and error on the run's evidence file, and the next backend
        from ``select_verify_backend`` (the requesting session's order, tried
        backends skipped) takes a new work run in the same child session. When
        no backend remains, the parent gets the failure report and the
        report's delivery archives the child, as a failed verify does.
        """
    session_id = meta.id
    answer: str | None = None
    if durable_outcome == "success":
      events_log = self._tree.runs.run_dir(session_id, run.id) / runs.RUN_EVENTS_NAME
      answer = await asyncio.to_thread(review._worker_summary_from_events_log, events_log)
      if answer is not None:
        await self._complete_cold_read(meta, run, answer)
        return
    error = "empty final answer" if durable_outcome == "success" else await self._cold_read_attempt_error(
        session_id, run)
    await self._record_cold_read_attempt(meta, run, error)
    work_runs = [r for r in self._tree.runs.list_run_records_sync(session_id) if r.kind == "work"]
    tried = [r.backend for r in work_runs if r.backend]
    selection = await spawner_backends.select_verify_backend(
        meta.task_parent_id or "", self._cfg, self._tree.session_store, tried, caller="cold_read")
    if selection is not None:
      resolved_backend, resolved_model, _tried = selection
      async with self._tree.control_lock:
        run_id = control_events.stable_run_id(session_id, f"cold-read:work:{len(work_runs)}")
        record = models.RunRecord(
            id=run_id,
            session_id=session_id,
            kind="work",
            backend=resolved_backend,
            model=resolved_model,
        )
        await self._tree.runs.register_run_locked(
            record, task_spec_text=task_sessions.canonical_task_spec_text(meta.task))
      self.launch(session_id, run_id)
      return
    body = await self._cold_read_report(meta, run, backend_id=None, answer=None)
    await self._report_failure_to_parent(session_id, run, "failed", summary=body)

  async def _complete_cold_read(self, meta: models.SessionMetadata, run: models.RunRecord, answer: str) -> None:
    """Write the success report body and close the child through the automatic completion."""
    body = await self._cold_read_report(meta, run, backend_id=run.backend, answer=answer)
    try:
      await self._tree.completion.evaluate_automatic_completion(meta.id, run_id=run.id, summary=body)
    except task_errors.TaskConflictError as e:
      log.warning(
          "cold_read_delivery_close_blocked", session_id=meta.id, run_id=run.id, blockers=getattr(e, "blockers", None))

  async def _failure_text(
      self,
      session_id: str,
      run: models.RunRecord,
      first: Callable[[pathlib.Path], str | None],
      second: Callable[[pathlib.Path], str | None],
  ) -> str:
    """The failed run's reportable text: the ``first`` reader's text, then the ``second``'s, else the bare fallback.

    Each reader streams the events log from its end and stops at the first settling event, so ``second`` runs only
    when ``first`` finds nothing.
    """
    events_log = self._tree.runs.run_dir(session_id, run.id) / runs.RUN_EVENTS_NAME
    if events_log.is_file():
      for read in (first, second):
        text = await asyncio.to_thread(read, events_log)
        if text:
          return text
    return f"run {run.id} failed without a reportable output"

  async def _cold_read_attempt_error(self, session_id: str, run: models.RunRecord) -> str:
    """The failed cold-read run's error for the report's attempt line: the run's error
    event, its closing text when no error event exists, or the bare fallback."""
    return await self._failure_text(
        session_id, run, review._worker_error_from_events_log, review._worker_summary_from_events_log)

  async def _record_cold_read_attempt(self, meta: models.SessionMetadata, run: models.RunRecord, error: str) -> None:
    """Record one failed cold-read attempt on the run's evidence file (the report reads it back)."""
    cold = meta.task.cold_read
    assert cold is not None
    evidence_path = self._tree.runs.run_dir(meta.id, run.id) / cold_read.EVIDENCE_FILENAME
    evidence: dict = {"page_path": cold.page_path, "page_sha256": None}
    if evidence_path.is_file():
      evidence = json.loads(await asyncio.to_thread(evidence_path.read_text, encoding="utf-8"))
    evidence["error"] = error
    await asyncio.to_thread(json_utils.atomic_write_text, evidence_path, json.dumps(evidence))

  async def _cold_read_report(
      self, meta: models.SessionMetadata, run: models.RunRecord, *, backend_id: str | None, answer: str | None) -> str:
    """The report body for one settled cold-read: the page and its sha256, every failed
    attempt in run order, then the answering backend and its answer — or the all-failed
    trailer. The sha256 comes from the settling run's evidence file: what that run read."""
    cold = meta.task.cold_read
    assert cold is not None
    session_id = meta.id
    attempts: list[tuple[str, str]] = []
    page_sha256: str | None = None
    for record in self._tree.runs.list_run_records_sync(session_id):
      if record.kind != "work":
        continue
      evidence_path = self._tree.runs.run_dir(session_id, record.id) / cold_read.EVIDENCE_FILENAME
      if not evidence_path.is_file():
        continue
      evidence = json.loads(await asyncio.to_thread(evidence_path.read_text, encoding="utf-8"))
      if record.id == run.id:
        page_sha256 = evidence.get("page_sha256")
      if "error" in evidence and record.backend:
        attempts.append((record.backend, evidence["error"]))
    if page_sha256 is None:
      # The settling run never read the page (its launch failed on the read):
      # state the page as it stands at report time.
      page_bytes = await asyncio.to_thread(pathlib.Path(cold.page_path).read_bytes)
      page_sha256 = hashlib.sha256(page_bytes).hexdigest()
    return cold_read.format_report(cold.page_path, page_sha256, attempts, backend_id, answer)

  async def _report_failure_to_parent(
      self,
      session_id: str,
      run: models.RunRecord,
      outcome: str,
      *,
      summary: str | None = None,
      deciding_run: models.RunRecord | None = None,
  ) -> None:
    """Persist one failed/blocked child_report with stable source/recipient evidence.

        The source event is the Run's durable run_finished fact and the
        recipient is the close-time fixed parent, so the stable report id
        dedups across recovery and repeated finalize. Only a freshly created
        report wakes the parent, so a recovery re-delivery of an already
        delivered report never wakes twice. Once the report is delivered, the
        worker's archive entry runs: it archives the node when it provably
        stopped, and delivers no report of its own. ``deciding_run`` names
        the run the archive judges — the review run whose own ending decided
        the outcome — when the report names another run.
        """
    meta = await self._tree.load_meta(session_id)
    if meta is None or not meta.task_parent_id:
      return
    events = self._tree.runs.load_events_sync(session_id)
    source = next((e for e in reversed(events) if e.get("type") == ET.RUN_FINISHED and e.get("run_id") == run.id), None)
    if source is None:
      return
    if summary is None:
      summary = await self._worker_failure_summary(session_id, run)
    parent_id = str(meta.task_parent_id)
    parent_epoch = await self._tree.session_events.prime_aggregator(parent_id)
    async with self._tree.control_lock:
      if self._tree.task_state(session_id) != "open":
        return
      report, report_created = await self._tree.dispatch.deliver_child_report_locked(
          session_id,
          source_event=source,
          outcome=outcome,
          summary=summary,
          result_refs=[f"{task_completion.RUN_REF_PREFIX}{run.id}"],
          recipient=meta.task_parent_id,
      )
    if report_created:
      await self._tree.dispatch.finish_report_delivery(
          parent_id, report, epoch=parent_epoch, child_session_id=session_id)
    archive_run = deciding_run if deciding_run is not None else run
    await self._tree.completion.archive_unsuccessful_worker(session_id, run_id=archive_run.id, outcome=outcome)

  async def _worker_failure_summary(self, session_id: str, run: models.RunRecord) -> str:
    """The failed run's own closing words (or its error, or the bare outcome).

        The summary must name the actual error when the run produced no report:
        a run that died before its process started carries its error as the
        events log's error event, and that text is the report's evidence.
        """
    return await self._failure_text(
        session_id, run, review._worker_summary_from_events_log, review._worker_error_from_events_log)


# The process owner of the task tree; built on the first ``task_manager()`` call.
_task_manager: task_sessions.TaskTreeManager | None = None


def task_manager() -> task_sessions.TaskTreeManager:
  """The task-tree owner singleton; it owns the control lock the runs.RunStore shares.

  Construction installs the execution adapter as the input dispatcher's
  executor — the application initialization owner wiring durable dispatch to
  actual manager/worker/review execution. A test-built TaskTreeManager keeps
  its executor None until it installs one.
  """
  global _task_manager
  if _task_manager is None:
    _task_manager = task_sessions.TaskTreeManager(
        config.get_config(),
        session_store.store(),
        session_events.events(),
        session_sidebar.sidebar(),
        session_listing.listing(),
        session_lifecycle.lifecycle(),
        session_fork.fork(),
        session_anchors.anchors(),
        session_successor.successor(),
    )
    _task_manager.dispatch.executor = TaskExecutionAdapter(
        config.get_config(),
        session_successor.successor(),
        session_events.events(),
        session_lifecycle.lifecycle(),
        session_anchors.anchors(),
        session_sidebar.sidebar(),
        _task_manager,
    )
  return _task_manager


def run_store() -> runs.RunStore:
  return task_manager().runs


def set_task_manager(mgr: task_sessions.TaskTreeManager | None) -> None:
  """Replace the task-tree owner singleton (tests); None restores lazy construction."""
  global _task_manager
  _task_manager = mgr


async def _reconcile_node(
    session_id: str,
    tree: task_sessions.TaskTreeManager,
    adapter: TaskExecutionAdapter | None,
    counters: dict,
    cfg: config.CharlieBotConfig,
    is_driven: Callable[[models.RunRecord], bool] | None = None,
) -> None:
  """Reconcile one node from its durable facts.

    ``is_driven`` marks the runs this process is already driving (an execute
    task in flight, or a resume follow). Boot reconcile omits it — its premise
    is that no follower exists in this process yet — while the end-landing
    retry passes it: the retry runs while the server is live, so steps 1 and 2
    must skip runs whose driver this process already holds (steps 3 and 4 stay
    unskipped: they dedupe by stable ids).
    """
  meta = await tree.load_meta(session_id)
  if meta is None:
    return
  events = tree.runs.load_events_sync(session_id)
  run_records = tree.runs.list_run_records_sync(session_id)
  # Warm the display-backend map: a fresh boot has seen no Run liveness
  # notification, and the sidebar row and header badge show this node's
  # newest Run's backend (the persisted metadata.backend is never rewritten).
  newest = tree.runs.newest_run_record_sync(session_id)
  if newest is not None:
    thinking_state.note_run_backend(session_id, newest.backend)

  # --- 0. an interrupted prompt edit lands its missing fact --------------
  # Idempotent: a landed fact appends nothing, so repeated recovery and
  # concurrent double-reconcile never duplicate a prompt_changed event.
  async with tree.control_lock:
    locked_meta = await tree.load_meta(session_id)
    if locked_meta is not None:
      for scope in ("subtree", "node"):
        await tree._ensure_prompt_changed_fact(session_id, locked_meta, scope)

  # --- 1. stop requests take precedence over any launch or follow --------
  for run in run_records:
    if is_driven is not None and is_driven(run):
      continue  # this process already drives it: its driver owns the stop
    if tree.runs.run_has_terminal_fact(run, events):
      continue
    if tree.runs.stop_requested(events, run.id):
      # The owner's reconcile: a queued run never launches (it keeps the
      # request and claims nothing); a launched run is identity-checked,
      # signalled, and its observed exit lands as the terminal fact.
      await tree.runs.reconcile_stop_request(session_id, run.id)

  # A queued run on a non-open node never launches, and without a stop
  # request it sits queued forever and blocks the node's archive. The
  # request id is the launch path's, so a run both paths settle carries one
  # fact; the pass records it before the replay and dispatch below run.
  if tree.task_state(session_id) != "open":
    for run in run_records:
      if tree.runs.run_has_terminal_fact(run, events):
        continue
      if tree.runs.stop_requested(events, run.id):
        continue
      await tree.runs.request_stop(session_id, run.id, closed_task_stop_request_id(run.id))
      events = tree.runs.load_events_sync(session_id)

  # --- 2. launched, non-terminal Runs re-attach or drain -----------------
  for run in run_records:
    if is_driven is not None and is_driven(run):
      continue  # this process already drives it: never a second follower
    if run.pid is None:
      continue  # queued (never launched): step 4's dispatch decides
    if tree.runs.run_has_terminal_fact(run, events):
      continue
    if tree.runs.stop_requested(events, run.id):
      continue  # step 1 already drove this stop through
    if adapter is None:
      raise RuntimeError("task execution adapter is not installed; cannot reconcile a launched run")
    if runs.is_run_alive(run.pid, run.pid_start, run.started_at, runs.read_host_boot_time()):
      counters["resumed"] += 1
      log.info("task_recovery_resume", session=session_id, run_id=run.id)
      # The follow MUST NOT be awaited inline: a live run's tail-follow
      # ends only when its process ends, so awaiting it here would hold
      # the whole server startup (the lifespan awaits this pass) until
      # every live v2 run finished. Scheduling it attaches the follow
      # before any door opens, and the recorded pid keeps holding the
      # node's serialized slot (no competing dispatch) until the follow
      # converges and lands the run's durable terminal fact.
      adapter.follow_run_in_background(session_id, run.id)
    else:
      # The process ended before its terminal fact landed (crash in the
      # follow). The drain converges to the durable result — raw-stream
      # truth where it exists, an honest interrupted/failed outcome
      # where it does not. It is never relaunched.
      counters["drained"] += 1
      log.warning("task_recovery_drain", session=session_id, run_id=run.id)
      await adapter.resume_run(session_id, run.id, is_alive=lambda: False)

  # --- 3. terminal Runs replay their missing follow-up once --------------
  await _replay_followups(session_id, tree, adapter, counters, cfg)

  # --- 4. pending reports and pending inputs -----------------------------
  await tree.dispatch.recover_pending_reports(session_id)
  # The dispatch is the deterministic repair for admitted-but-unclaimed
  # input (a reservation whose claim was lost) and for queued work: the
  # same launch checks every fresh dispatch passes, no new message needed.
  await tree.dispatch.dispatch_pending(session_id)


def _no_reviewer_summary(run: models.RunRecord, cfg: config.CharlieBotConfig) -> str:
  """The blocked report a review-less finish sends its parent: the pool's per-member recovery times.

  The live finish path and the startup replay both send this report, and the
  replay must reproduce the live chain's summary text.
  """
  return (
      f"review of work run {run.id} found no available reviewer backend; "
      f"backends.pool: {backend_pool.recovery_report(cfg)}")


def _replay_owes_review(
    tree: task_sessions.TaskTreeManager,
    session_id: str,
    run_records: list[models.RunRecord],
    run: models.RunRecord,
) -> bool:
  """Whether the startup replay owes a successful implement work run its review.

  The live finish path's verdict, re-derived at replay time: the task is
  still open, and no other work run followed this one. A work run that
  started later followed; a registered work run with no ``started_at``
  followed too — the live path read its reservation as a pause — even
  though the run list sorts it first (``runs._run_sort_key``), so the list
  position alone never answers this.
  """
  if tree.task_state(session_id) != "open":
    return False
  for other in run_records:
    if other.kind != "work" or other.id == run.id:
      continue
    if other.started_at is None:
      return False
    if run.started_at is None or (other.started_at, other.id) > (run.started_at, run.id):
      return False
  return True


async def _replay_followups(
    session_id: str,
    tree: task_sessions.TaskTreeManager,
    adapter: TaskExecutionAdapter | None,
    counters: dict,
    cfg: config.CharlieBotConfig,
) -> None:
  """Re-drive every completed Run's follow-up that its crash window lost.

    Every follow-up here is idempotent by construction: reviews are
    provenance-deduped, sequence advance is facts-driven over stable Run ids,
    reports and closes ride stable ids, so a repeated pass lands nothing
    twice.
    """
  meta = await tree.load_meta(session_id)
  if meta is None:
    return
  events = tree.runs.load_events_sync(session_id)
  run_records = tree.runs.list_run_records_sync(session_id)
  for run_index, run in enumerate(run_records):
    outcome = tree.runs.terminal_outcome(events, run.id)
    # A terminal run whose metadata write failed (ended_at empty) gets its
    # end metadata re-derived from the raw log: the run_finished fact is on
    # disk, ended_at/exit_code are not. Idempotent — an already-written
    # metadata file is never touched.
    if outcome is not None and run.ended_at is None:
      if adapter is None:
        raise RuntimeError("task execution adapter is not installed; cannot repair a half-written end record")
      await adapter.repair_end_metadata(session_id, run, outcome)
    if run.sequence_ref is not None:
      controller = sequence_controllers.controller_for_sequence(run.sequence_ref)
      if controller is None:
        # The owning package is gone: a launched Run was already followed or
        # drained above, and a registered-but-unlaunched one would sit queued
        # forever (the dispatcher never launches a sequence Run), so it lands
        # an interrupted finish through the run store's normal entry. A
        # terminal Run only logs; a later pass finds the fact and logs again.
        if outcome is None and run.pid is None:
          await tree.runs.record_finish(session_id, run.id, "interrupted")
        if outcome is not None or run.pid is None:
          log.warning(
              "sequence_run_without_controller",
              session_id=session_id,
              run_id=run.id,
              owner_ref=run.sequence_ref.owner_ref)
        continue
      if await controller.recover_run(session_id, run, outcome, tree, cfg):
        counters["followups"] += 1
      continue
    if outcome is None:
      continue
    if meta.profile != "worker" and run.kind != "manager_turn":
      continue
    if meta.profile == "manager" and run.kind == "manager_turn":
      # The durable write happened, the crash landed before
      # after_run_finished: the owner-close recheck replays once (its
      # request-id replay dedups, and so does its blocked-request
      # notice); phase 4's dispatch covers the rest.
      counters["followups"] += 1
      await tree.completion.recheck_close_requests(session_id, run.id)
      continue
    if run.kind == "work":
      counters["followups"] += 1
      if meta.task is not None and meta.task.task_type == models.TaskType.COLD_READ:
        # The execution layer owns a cold-read Run's delivery (the report
        # body, the next-backend retry, the all-failed report), and the
        # generic completion skips the task type, so the replay re-drives
        # the chain here. A success re-drives the finish path's own
        # close-request recheck first, exactly as the live finish ran it. A
        # failed Run re-drives only when no retry Run follows it: a
        # registered retry's own finish drives the rest, and step 4's
        # dispatch launches a queued one. An interrupted Run reports through
        # the generic failure entry, exactly as the live chain's fall-through
        # does — never through _after_cold_read_run, whose next-backend
        # retry belongs to failed Runs.
        later_work = [r for r in run_records[run_index + 1:] if r.kind == "work"]
        if outcome == "success":
          await tree.completion.after_run_finished(session_id, run.id)
          await adapter._after_cold_read_run(meta, run, outcome)
        elif outcome == "failed" and not later_work:
          await adapter._after_cold_read_run(meta, run, outcome)
        elif outcome == "interrupted":
          await adapter._report_failure_to_parent(session_id, run, outcome)
        continue
      if outcome == "success":
        # The completion owner's post-success follow-up replays exactly
        # as the dispatcher's finish path ran it: the own close-request
        # recheck and the non-implement automatic completion (close +
        # parent report). Everything inside is idempotent by stable
        # request/close/report ids, so a crash in the finish→follow-up
        # window is repaired and a repeated pass lands nothing twice.
        await tree.completion.after_run_finished(session_id, run.id)
        if meta.task is not None and meta.task.task_type == models.TaskType.IMPLEMENT and run.repo_path:
          pause = tree.completion.pause_reason(session_id)
          if pause is not None:
            log.info("worker_delivery_paused", session_id=session_id, run_id=run.id, reason=pause)
          elif _replay_owes_review(tree, session_id, run_records, run):
            review_id = await adapter._maybe_spawn_review(session_id, run)
            if review_id is None:
              # The live finish path's own no-reviewer report, replayed: the pool has no member
              # the reviewer may use, and the report's stable id dedups against the live chain's.
              await adapter._report_failure_to_parent(
                  session_id, run, "blocked", summary=_no_reviewer_summary(run, cfg))
      elif outcome in ("failed", "interrupted", "blocked"):
        await adapter._report_failure_to_parent(session_id, run, outcome)
    elif run.kind == "review":
      counters["followups"] += 1
      await adapter._after_review_run(meta, run, outcome)
