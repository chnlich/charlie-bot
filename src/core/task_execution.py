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
import functools
import json
import time
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING

from src.agents.worker import QuotaExhaustedError, Worker
from src.core import claude_relay, git, review, runs, task_prompts
from src.core import event_types as ET
from src.core.chat_events import chat_events_path
from src.core.config import CharlieBotConfig, get_credentials
from src.core.constants import SESSION_ID_ENV_VAR, BackendType
from src.core.control_events import sha256_hex, stable_run_id
from src.core.log_once import LazyStructlogLogger
from src.core.models import (
    BackendOption,
    RunRecord,
    SessionMetadata,
    TaskType,
)
from src.core.run_token import RUN_TOKEN_ENV, RunTokenClaims, sign_run_token
from src.core.runs import RunNotFoundError, scan_result_exit
from src.core.sessions import SessionManager
from src.core.spawner_backends import resolve_backend_option
from src.core.takeoff_gate import DelegationBlockedError
from src.core.task_prompts import PromptSnapshot, TaskPromptError
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
            parts.append(
                f"[Report from task {event.get('child_session_id')} | outcome {event.get('outcome')}] {content}")
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
    tree: TaskTreeManager, index: object, meta: SessionMetadata,
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
            task_prompts.build_segments, cfg, fresh_meta, kind,
            overlay=overlay, chain=chain, node_ref=node_ref)
        candidate = task_prompts.assemble_snapshot(built)
        # Mutable-source fingerprint recheck: re-assemble and compare. Any
        # template/host/overlay/memory change between the two passes means no
        # coherent view existed yet.
        rebuilt, err2 = await asyncio.to_thread(
            task_prompts.build_segments, cfg, fresh_meta, kind,
            overlay=overlay, chain=chain, node_ref=node_ref)
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
            meta = await tree.load_meta(session_id)
            tree._require_task(meta, session_id)
            assert meta is not None
            if tree.task_state(session_id) != "open" or meta.automation_paused:
                return None
            if launch_run_id is not None:
                run = await tree.runs.get_run(session_id, launch_run_id)
                if run is None:
                    raise TaskNotFoundError(f"run {launch_run_id} not found in task {session_id}")
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
                request_id = "dispatch:" + sha256_hex(
                    "\x00".join(sorted(str(e.get("id")) for e in pending_now)))
                run_id = stable_run_id(session_id, request_id)
                existing = await tree.runs.get_run(session_id, run_id)
                if existing is None:
                    kind = "manager_turn" if meta.profile == "manager" else "work"
                    record = RunRecord(
                        id=run_id, session_id=session_id, kind=kind,  # type: ignore[arg-type]
                        backend=backend, model=model)
                    # This lock hold already covers the reservation: the locked
                    # registration variant, then the locked batch claim.
                    await tree.runs.register_run_locked(
                        record, task_spec_text=canonical_task_spec_text(meta.task))
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

    def launch_scheduled(self, session_id: str, run_id: str, *, prompt: str | None = None) -> None:
        """The configured scheduler's launch seam: a server-owned fire.

        The scheduler (and only in-process server code) reaches this; the run's
        launch authorization is the existing scheduled execution authorization
        rather than the nearest-user gate, exactly like the legacy scheduled
        worker entry this replaces.
        """
        key = (session_id, run_id)
        if key in self._launch_inflight:
            return
        self._launch_inflight.add(key)
        self._arm_launch_settlement(key)
        self._schedule_launch(session_id, run_id, prompt=prompt, scheduled=True)

    def _schedule_launch(
        self, session_id: str, run_id: str, *, prompt: str | None = None,
        scheduled: bool = False,
    ) -> None:
        """Fire-and-forget the actual execution; the reservation is already durable."""
        from src.core.tasks import create_logged_task

        async def _execute_and_release() -> None:
            key = (session_id, run_id)
            try:
                verdict = await self.execute_run(
                    session_id, run_id, launch_prompt=prompt, scheduled=scheduled)
            except Exception as exc:
                # The launch died before the run's execution could land a
                # terminal fact: the waiting controller must see the actual
                # failure instead of polling forever behind it. The exception
                # still propagates (the logging task owner records it).
                self._settle_launch(key, f"failed-to-start: {exc}")
                raise
            finally:
                self._launch_inflight.discard(key)
            self._settle_launch(key, verdict)

        create_logged_task(_execute_and_release(), name=f"task-run-{run_id[:8]}")

    async def execute_run(
        self, session_id: str, run_id: str, *, launch_prompt: str | None = None,
        scheduled: bool = False,
    ) -> str:
        """Pre-launch rechecks, then execute one Run on its kind's adapter.

        Rechecks run under the control lock immediately before the launch:
        role (the kind/profile pairing), open ancestors, pause, authorization
        and any durable stop request. The backend resolution afterwards is
        explicit — a missing or invalid backend fails visibly, never
        silently substituted. ``launch_prompt`` is the sequence controllers'
        explicit launch text; see :meth:`launch`. ``scheduled`` marks a fire
        the configured scheduler owns (its server-side invocation is the
        existing scheduled execution authorization, with provenance the
        server derived itself); every other launch re-judges the
        nearest-real-user-ancestor gate at the actual start.

        Returns ``LAUNCH_STARTED`` when the Run's execution adapter was
        entered, or the actual refusal reason when a launch precondition
        withheld it (no process started and no terminal fact will arrive).
        An exception escaping before the adapter was entered is a
        failed-to-start launch: the caller's settlement sees it, never an
        endless wait.
        """
        tree = self._tree
        async with tree.control_lock:
            meta = await tree.load_meta(session_id)
            tree._require_task(meta, session_id)
            assert meta is not None
            run = await tree.runs.get_run(session_id, run_id)
            if run is None:
                raise RunNotFoundError(f"run {run_id} not found in task {session_id}")
            state = tree.task_state(session_id)
            if state != "open" or meta.automation_paused:
                reason = (f"withheld: task {session_id} is {state}" if state != "open"
                          else f"withheld: task {session_id} is paused")
                log.info("run_launch_withheld", session_id=session_id, run_id=run_id, reason=reason)
                return reason
            await tree._require_open_ancestry(session_id)
            events = tree.runs.load_events_sync(session_id)
            if tree.runs.run_has_terminal_fact(run, events):
                log.info("run_launch_refused_by_facts", session_id=session_id, run_id=run_id)
                return f"refused: run {run_id} already finished"
            if tree.runs.stop_requested(events, run_id):
                log.info("run_launch_refused_by_facts", session_id=session_id, run_id=run_id)
                return f"refused: run {run_id} has a durable stop request"
            if (meta.profile == "worker" and run.kind in ("work", "review") and meta.task_parent_id
                    and not scheduled and not self._verify_exempt(meta)):
                # The nearest-user-ancestor gate re-judges at actual launch
                # (plan 4.2: pending execution requests re-judge where they
                # start). A configured scheduler fire runs under the existing
                # scheduled execution authorization instead (see
                # launch_scheduled), and the read-only verify exemption rides
                # the same task-type judgment the delegation route applies.
                try:
                    await tree.check_task_authorization(meta.task_parent_id)
                except DelegationBlockedError as e:
                    log.info("run_launch_authorization_withheld",
                             session_id=session_id, run_id=run_id, reason=str(e))
                    return f"withheld: {e}"
        option = self._resolve_run_backend(run)
        # The one assembly owner builds this launch's managed instructions and
        # their durable snapshot BEFORE any backend is invoked. A preparation
        # failure (missing/corrupt rule, malformed memory, unsettled sources)
        # is a definitely-unlaunched withheld verdict: the queued Run and its
        # unconsumed inputs stay exactly as they were.
        try:
            snapshot = await self._prepare_launch_snapshot(meta, run, option)
        except TaskPromptError as e:
            log.error("task_prompt_preparation_failed", session_id=session_id, run_id=run_id,
                      kind=run.kind, error=str(e))
            return f"withheld: prompt preparation failed: {e}"
        if meta.profile == "manager" and run.kind == "manager_turn":
            await self._execute_manager_turn(meta, run, option, snapshot)
        elif meta.profile == "worker" and run.kind in ("work", "review", "iteration", "scheduled_step"):
            await self._execute_worker_run(meta, run, option, snapshot, launch_prompt=launch_prompt)
        else:
            raise TaskInvalidError(
                f"run {run_id} (profile={meta.profile}, kind={run.kind}) has no executable adapter")
        return LAUNCH_STARTED

    @staticmethod
    def _verify_exempt(meta: SessionMetadata) -> bool:
        """The established read-only verify exemption: a verify task's run
        never needs a takeoff window (the same exemption the delegation route
        applies at admission)."""
        return meta.task is not None and meta.task.task_type == TaskType.VERIFY

    async def _await_terminal(self, session_id: str, run_id: str) -> str:
        """Await one Run's durable terminal fact; returns its outcome."""
        tree = self._tree
        while True:
            run = await tree.runs.get_run(session_id, run_id)
            events = tree.runs.load_events_sync(session_id)
            outcome = tree.runs.terminal_outcome(events, run_id) if run is not None else None
            if outcome is not None:
                return outcome
            await asyncio.sleep(0.2)

    async def launch_and_settle(
        self, session_id: str, run_id: str, *, prompt: str | None = None,
        scheduled: bool = False,
    ) -> LaunchSettlement:
        """The sequence controllers' shared launch/wait observation.

        Launches one registered Run (or joins the in-flight launch) and
        settles to either its durable terminal outcome or an explicit
        withheld verdict. Lifecycle facts decide which: a Run that already
        carries a terminal fact returns it; a live or ended process is
        followed to its fact (never relaunched, never killed for running
        long); only a launch whose precondition failed in the
        registration-to-launch interval — node closed/paused, durable stop,
        expired authorization, startup failure — settles withheld, with the
        actual reason. The first terminal fact always wins over a settle
        race.
        """
        tree = self._tree
        run = await tree.runs.get_run(session_id, run_id)
        if run is None:
            raise RunNotFoundError(f"run {run_id} not found in task {session_id}")
        events = tree.runs.load_events_sync(session_id)
        outcome = tree.runs.terminal_outcome(events, run_id)
        if outcome is not None:
            return LaunchSettlement(outcome=outcome)
        if run.pid is not None:
            # A process this launch must never duplicate (a replayed
            # registration, or a follow that outlives the controller): follow
            # it to its durable fact.
            return LaunchSettlement(outcome=await self._await_terminal(session_id, run_id))
        key = (session_id, run_id)
        if key not in self._launch_settlements:
            if scheduled:
                self.launch_scheduled(session_id, run_id, prompt=prompt)
            else:
                self.launch(session_id, run_id, prompt=prompt)
        future = self._launch_settlements.get(key)
        if future is None:
            # The launch settled (and dropped its entry) between the facts
            # read above and now: re-read what actually happened.
            events = tree.runs.load_events_sync(session_id)
            outcome = tree.runs.terminal_outcome(events, run_id)
            if outcome is not None:
                return LaunchSettlement(outcome=outcome)
            raise RuntimeError(f"run {run_id} settled without a terminal fact or launch verdict")
        verdict = await future
        if verdict != LAUNCH_STARTED:
            # First terminal fact wins over a settle race: the run may have
            # finished through another path while this verdict was formed.
            events = tree.runs.load_events_sync(session_id)
            outcome = tree.runs.terminal_outcome(events, run_id)
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
        key = str(get_credentials().get("charliebot", "access_key") or "")
        if not key:
            raise RuntimeError(
                "run-token signing requires credentials.yaml charliebot.access_key; "
                "agent child environments cannot be built without it")
        token = sign_run_token(
            RunTokenClaims(session_id=session_id, run_id=run_id, agent=agent_name or "worker"), key)
        return {
            SESSION_ID_ENV_VAR: session_id,
            RUN_TOKEN_ENV: token,
            "CHARLIEBOT_HOME": str(self._cfg.charliebot_home),
        }

    # ------------------------------------------------------------------
    # Manager turns
    # ------------------------------------------------------------------


    async def _alert_overlay_inactive(
        self, session_id: str, option: BackendOption, overlay_error: OSError | None,
        declared: bool,
    ) -> None:
        """The unified fenceless-run alert (undeclared or unreadable overlay)."""
        reason = "unreadable" if overlay_error is not None else "undeclared"
        log.warning("task_overlay_inactive", session_id=session_id, backend=option.id, reason=reason)
        await self._sessions.callbacks().persist_and_broadcast(session_id, {
            "type": ET.BACKEND_OVERLAY_INACTIVE,
            "backend": option.id,
            "reason": reason,
            **({"overlay": option.prompt_overlay,
                "error": type(overlay_error).__name__} if overlay_error is not None else {}),
        })

    async def _prepare_launch_snapshot(
        self, meta: SessionMetadata, run: RunRecord, option: BackendOption,
    ) -> PromptSnapshot:
        """Build, recheck, and durably commit this launch's instruction snapshot.

        The committed bytes are the bytes the adapters launch — never a second
        build. A declared-but-unreadable or undeclared overlay emits the unified
        fenceless-run alert exactly like the v1 wake path.
        """
        snapshot, overlay_error, declared = await assemble_coherent_snapshot(
            self._cfg, self._tree, meta, run.kind, option)
        path = self._tree.runs.run_dir(meta.id, run.id) / task_prompts.SNAPSHOT_FILENAME
        from src.core.json_utils import atomic_write_text
        await asyncio.to_thread(
            atomic_write_text, path,
            json.dumps(snapshot.to_json_dict(), indent=2, ensure_ascii=False))
        await self._tree.runs.record_observation(meta.id, run.id, prompt_snapshot_ref=str(path))
        if not declared or overlay_error is not None:
            await self._alert_overlay_inactive(meta.id, option, overlay_error, declared)
        return snapshot

    async def _execute_manager_turn(
        self, meta: SessionMetadata, run: RunRecord, option: BackendOption, snapshot: PromptSnapshot,
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

        anchor_continues = (
            meta.cc_session_id is not None
            and meta.native_prompt_hash == snapshot.prompt_hash
            and meta.native_backend == option.id
            and meta.native_model == option.model)
        fresh_native = not anchor_continues
        prompt = content
        if fresh_native and meta.cc_session_id is not None:
            # A reset, not a first turn: name the task and where the earlier
            # history lives, so the fresh native context can catch up without
            # the old conversation being copied or destroyed.
            goal = meta.task.goal if meta.task is not None else meta.name
            prompt = (
                f"[Context reset: this task's managed instructions or sources changed since the "
                f"previous turn, so this turn starts a fresh native conversation. The task is: "
                f"{goal}. Earlier turns' history remains readable in session {session_id}'s chat "
                f"log and Run records (GET /api/sessions/{session_id}/runs).]\n\n{content}")

        async def on_task_spawn(pid: int, pid_start: str | None) -> None:
            if pid_start is None:
                raise RuntimeError(f"run {run_id} spawned without a pinned pid_start")
            await self._tree.runs.record_launch(session_id, run_id, pid=pid, pid_start=pid_start)
            # The anchor decision is durable the moment the process exists: a
            # reset clears the anchor here (never during preparation, which may
            # fail without touching the usable old anchor), and the identity
            # fields pin the snapshot this conversation continues under.
            await self._tree.record_native_anchor(
                session_id, prompt_hash=snapshot.prompt_hash, backend=option.id,
                model=option.model, reset_anchor=fresh_native)

        async def on_task_finish(cc_session_id: str | None, exit_code: int, finish_extras: dict) -> None:
            await self._tree.runs.record_observation(
                session_id, run_id,
                native_session_id=cc_session_id,
                model=finish_extras.get("model") or option.model,
                raw_log_ref=str(transport_dir / runs.RAW_LOG_NAME),
                result_ref=str(transport_dir / runs.RAW_LOG_NAME),
            )
            await self._tree.dispatch.finish_run(
                session_id, run_id,
                outcome="success" if exit_code == 0 else "failed",
                exit_code=exit_code,
            )
            # Inputs admitted during this turn waited for the serialized
            # consumer; the turn's finish is what dispatches their next run.
            await self._tree.dispatch.dispatch_pending(session_id)

        await self._persist_launch_text(session_id, run_id, prompt)
        log.info("manager_turn_launching", session_id=session_id, run_id=run_id,
                 backend=option.id, inputs=len(run.input_event_ids),
                 prompt_hash=snapshot.prompt_hash[:12], fresh_native=fresh_native)
        await run_message(
            self._cfg,
            meta,
            prompt,
            self._sessions.callbacks(),
            skip_user_event=True,
            auto_trigger=any(e.get("type") == ET.SCHEDULED_TRIGGER for e in batch_events),
            backend_option=option,
            uploaded_files=uploaded_files or None,
            expect_fresh_session=fresh_native,
            task_instructions=snapshot.instructions_text,
            task_run=TaskRunBinding(
                session_id=session_id, run_id=run_id, transport_dir=str(transport_dir),
                fresh_native_context=fresh_native),
            on_task_spawn=on_task_spawn,
            on_task_finish=on_task_finish,
            extra_env=self._child_env(session_id, run_id, meta.name),
        )


    # ------------------------------------------------------------------
    # Worker work and review runs
    # ------------------------------------------------------------------

    async def _execute_worker_run(
        self, meta: SessionMetadata, run: RunRecord, option: BackendOption, snapshot: PromptSnapshot,
        *, launch_prompt: str | None = None,
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
        events_log = run_dir / "events.jsonl"

        task = meta.task
        task_type = task.task_type if task is not None else TaskType.IMPLEMENT
        review_worktree: str | None = None
        if run.kind == "review":
            work_run = await self._tree.runs.get_run(session_id, run.review_of_run_id or "")
            if work_run is None:
                raise TaskInvalidError(f"review run {run_id} names no recorded work Run")
            # The review reuses the work Run's exact repo, branch and worktree.
            review_worktree = work_run.worktree_path
            context = await self._build_review_context(session_id, run, work_run)
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
            await self._tree.runs.record_launch(
                session_id, run_id, pid=spawned.pid, pid_start=spawned.pid_start)

        error = ""
        exit_code = -1
        worker: Worker | None = None
        try:
            working_dir = Path(review_worktree) if review_worktree else run_dir
            worker = Worker(
                binding,  # type: ignore[arg-type]
                working_dir,
                events_log,
                prompt,
                self._cfg,
                backend_option=option,
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
        except QuotaExhaustedError as exc:
            if worker is not None:
                await worker.terminate()
            error = str(exc)
            log.warning("task_run_quota_exhausted", session_id=session_id, run_id=run_id, error=error)
        except Exception as exc:  # setup/transport failure: the run failed loudly
            log.error("task_run_failed", session_id=session_id, run_id=run_id,
                      error=str(exc), exc_info=True)
            if worker is not None:
                await worker.terminate()
            error = str(exc)

        durable_outcome = await self._finalize_worker_run(
            meta, run, option, exit_code=exit_code, error=error)
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
    ) -> str:
        """Land one worker Run's observation and terminal fact; returns the durable outcome.

        Successful process exit is only one input: the durable outcome requires
        a successful result event in the run's raw transport log — empty
        output or a missing result keeps the task at attention with the
        evidence retained. The first terminal fact wins: a stop request that
        observed the exit first stands, and this finish reconciles against it.
        """
        session_id, run_id = meta.id, run.id
        run_dir = self._tree.runs.run_dir(session_id, run_id)
        raw_path = run_dir / runs.RAW_LOG_NAME
        native_session_id = await self._native_session_id(raw_path, option)
        outcome = await self._worker_outcome(run_dir, option)
        await self._tree.runs.record_observation(
            session_id, run_id,
            native_session_id=native_session_id,
            model=option.model,
            raw_log_ref=str(raw_path),
            events_ref=str(run_dir / "events.jsonl"),
            result_ref=str(raw_path),
        )
        await self._tree.dispatch.finish_run(
            session_id, run_id, outcome=outcome, exit_code=exit_code if not error else -1)
        durable = self._tree.runs.terminal_outcome(
            self._tree.runs.load_events_sync(session_id), run_id) or outcome
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
        events_log = run_dir / "events.jsonl"
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
            base_branch, branch_name, worktree_path, start_point = await self._prepare_worktree(
                meta, run, repo_path)
        assert base_branch and branch_name and worktree_path
        origin = f"`{base_branch}`" + (f" @ `{start_point}`" if start_point else "")
        parts.append(task_prompts.render_worktree_bindings(
            self._cfg, task_type=task_type, intro_line=self._binding_intro(run),
            branch_name=branch_name, base_branch_origin=origin,
            wt_path=worktree_path, repo_path=str(repo_path)))
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
        parts.append(task_prompts.render_worktree_bindings(
            self._cfg, task_type=TaskType.IMPLEMENT, intro_line=self._binding_intro(run),
            branch_name=run.branch_name, base_branch_origin=f"`{run.base_branch}`",
            wt_path=run.worktree_path, repo_path=run.repo_path))
        parts.append(task_prompts.render_task_body(self._cfg, description))
        parts.append(task_prompts.render_iteration_reports(
            self._cfg, loop_dir=seq.owner_ref, iteration_number=seq.position))
        return "\n\n".join(part for part in parts if part)

    async def _build_step_context(
        self, meta: SessionMetadata, run: RunRecord, step_prompt: str, task_type: TaskType,
    ) -> str:
        """One scheduled step's context: the controller's prompt over the task's bindings.

        The step Run shares the leaf task's worktree provenance (pinned at
        registration); the controller's prompt is the task/input body.
        """
        parts = [task_prompts.render_session_info(self._cfg, meta.name)]
        if run.worktree_path and run.branch_name and run.base_branch and run.repo_path:
            parts.append(task_prompts.render_worktree_bindings(
                self._cfg, task_type=task_type, intro_line=self._binding_intro(run),
                branch_name=run.branch_name, base_branch_origin=f"`{run.base_branch}`",
                wt_path=run.worktree_path, repo_path=run.repo_path))
        parts.append(task_prompts.render_task_body(self._cfg, step_prompt))
        return "\n\n".join(part for part in parts if part)

    async def _build_review_context(self, session_id: str, run: RunRecord, work_run: RunRecord) -> str:
        """The review Run's task/input context: the work being judged and its git steps.

        The reviewer's stable contract rides the managed instructions
        (review_rules_text); this context names the exact work Run, its logs,
        and the volatile git steps — it never turns the reviewer into an
        implementer beyond the checklist's minimal-fix rule.
        """
        assert (work_run.branch_name and work_run.worktree_path and work_run.repo_path
                and work_run.base_branch), (
            f"review of work run {work_run.id} needs its exact repo/base/branch/worktree "
            "provenance; an unset base is never silently replaced with main")
        user_request, worker_summary = await review.extract_review_context(
            session_id, work_run.id, self._cfg.sessions_dir,
            worker_log_path=self._tree.runs.run_dir(session_id, work_run.id) / "events.jsonl")
        context_lines: list[str] = []
        if user_request:
            context_lines.append(f"**User request:** {user_request}")
        if worker_summary:
            context_lines.append(f"**Worker summary:** {worker_summary}")
        if not context_lines:
            context_lines.append("*(Log extraction unavailable — review based on delegator hint and diff only.)*")
        context_lines.append(f"**Delegator hint:** (work run {work_run.id})")
        return task_prompts.review_task_context(
            branch_name=work_run.branch_name,
            wt_path=work_run.worktree_path,
            base_branch=work_run.base_branch,
            session_id=session_id,
            chat_log_path=chat_events_path(self._cfg.sessions_dir / session_id),
            worker_log_path=self._tree.runs.run_dir(session_id, work_run.id) / "events.jsonl",
            context_section="\n".join(context_lines),
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

    async def _prepare_worktree(
        self, meta: SessionMetadata, run: RunRecord, repo_path: Path
    ) -> tuple[str, str, str, str | None]:
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
        resolution = await git.git_create_worktree(
            repo_path, base_branch, branch_name, wt_path, remote_tip=remote_tip)
        # The recorded base is the REQUESTED verification target exactly as
        # asked (an origin/ target verifies against the published tip); the
        # worktree's canonical start point stays in the creation resolution.
        await self._tree.runs.record_observation(
            meta.id, run.id,
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
        """
        tree = self._tree
        run = await tree.runs.get_run(session_id, run_id)
        if run is None:
            raise RunNotFoundError(f"run {run_id} not found in task {session_id}")
        if run.pid is None or run.pid_start is None:
            raise TaskInvalidError(
                f"run {run_id} records no launched process identity; nothing to re-attach")
        events = tree.runs.load_events_sync(session_id)
        if tree.runs.run_has_terminal_fact(run, events):
            return
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
        option = self._resolve_run_backend(run)
        if meta.profile == "manager" and run.kind == "manager_turn":
            await self._resume_manager_turn(meta, run, option, is_alive)
            return
        if meta.profile == "worker" and run.kind in ("work", "review", "iteration", "scheduled_step"):
            await self._resume_worker_run(meta, run, option, is_alive)
            return
        raise TaskInvalidError(f"run {run_id} (kind={run.kind}) has no resume adapter")

    async def _resume_manager_turn(
        self, meta: SessionMetadata, run: RunRecord, option: BackendOption, is_alive: Callable[[], bool]
    ) -> None:
        """Re-attach a v2 manager turn through the per-session queue's follow path."""
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
            raise RuntimeError(
                f"resume follow of run {run.id} must not spawn a process")

        async def on_task_finish(cc_session_id: str | None, exit_code: int, finish_extras: dict) -> None:
            await self._tree.runs.record_observation(
                meta.id, run.id,
                native_session_id=cc_session_id,
                model=finish_extras.get("model") or option.model,
                raw_log_ref=str(transport_dir / runs.RAW_LOG_NAME),
                result_ref=str(transport_dir / runs.RAW_LOG_NAME),
            )
            await self._tree.dispatch.finish_run(
                meta.id, run.id,
                outcome="success" if exit_code == 0 else "failed",
                exit_code=exit_code,
            )
            # Same serialized-input follow-up as a fresh manager turn.
            await self._tree.dispatch.dispatch_pending(meta.id)

        await enqueue_master_resume(
            self._cfg, meta, record, self._sessions.callbacks(),
            is_alive=is_alive,
            task_run=TaskRunBinding(session_id=meta.id, run_id=run.id, transport_dir=str(transport_dir)),
            on_task_spawn=on_task_spawn,
            on_task_finish=on_task_finish,
            extra_env=self._child_env(meta.id, run.id, meta.name),
        )

    async def _resume_worker_run(
        self, meta: SessionMetadata, run: RunRecord, option: BackendOption, is_alive: Callable[[], bool]
    ) -> None:
        """Re-attach a worker Run through Worker.resume's tail-follow."""
        session_id, run_id = meta.id, run.id
        run_dir = self._tree.runs.run_dir(session_id, run_id)
        events_log = run_dir / "events.jsonl"
        binding = RunWorkerBinding(
            id=run_id, session_id=session_id, pid=run.pid, pid_start=run.pid_start,
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
            meta, run, option, exit_code=exit_code, error="")
        await self._after_worker_run(meta, run, durable_outcome)

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
                if task_type == TaskType.IMPLEMENT and run.repo_path:
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
            # chain left pending. The review path reaches it too — its early
            # return used to strand inputs admitted during a review Run.
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
        work_run = work_run
        if durable_outcome == "failed":
            retried = await self._maybe_spawn_review(session_id, work_run)
            if retried is None:
                await self._report_failure_to_parent(
                    session_id, work_run, "blocked",
                    summary=f"review of work run {work_run.id} failed on every configured reviewer backend")
            return
        if durable_outcome != "success":
            return
        landing = await self._landing_for_work(work_run)
        if landing is None:
            await self._report_failure_to_parent(
                session_id, work_run, "blocked",
                summary=f"work run {work_run.id} passed review but its branch did not land on "
                        f"{work_run.base_branch or 'the requested base'}")
            return
        branch, commit, repo_path = landing
        refs = [f"{RUN_REF_PREFIX}{work_run.id}"]
        if work_run.task_spec_hash:
            refs.append(f"{SPEC_REF_PREFIX}{work_run.task_spec_hash}")
        refs.append(f"{REVIEW_REF_PREFIX}{run.id}")
        refs.append(f"{LANDING_REF_PREFIX}{branch}@{commit}")
        evidence = CompletionEvidence(
            summary=f"work run {work_run.id} delivered after review {run.id} landed {commit[:12]} on {branch}",
            result_refs=refs,
            run_ids=[work_run.id],
            review_run_ids=[run.id],
            landing=LandingEvidence(branch=branch, commit=commit, repo_path=repo_path),
        )
        try:
            await self._tree.completion.evaluate_automatic_completion(
                session_id, run_id=work_run.id, evidence=evidence)
        except TaskConflictError as e:
            log.warning("task_delivery_close_blocked",
                        session_id=session_id, run_id=work_run.id, blockers=getattr(e, "blockers", None))
        finally:
            await self._cleanup_worktree_if_delivered(session_id, work_run)

    async def _maybe_spawn_review(self, session_id: str, work_run: RunRecord) -> str | None:
        """Spawn the work Run's review on the same task, repo, branch and worktree.

        Idempotent by provenance: an existing non-terminal review of this work
        Run means one is already queued or running (recovery and repeated
        finalize never spawn a second). A failed reviewer retries down the
        existing preference policy with distinct Run records; exhausted
        retries keep the worktree and report blocked.
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
                if r.kind == "review" and r.review_of_run_id == work_run.id]
            if any(not tree.runs.run_has_terminal_fact(r, events) for r in existing):
                return existing[0].id
            attempts = len(existing)
            selection = review.select_reviewer_backend(
                self._cfg, work_run.backend or "", work_run.model,
                [r.backend for r in existing if r.backend])
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
            await tree.runs.register_run_locked(
                record, task_spec_text=f"review of work run {work_run.id}")
        self.launch(session_id, run_id)
        return run_id

    async def _landing_for_work(self, work_run: RunRecord) -> tuple[str, str, str] | None:
        """The (branch, commit, repo) landing evidence of a reviewed work Run, or None.

        The commit is the work branch's tip in its repository; the check
        requires that commit to exist and be an ancestor of the requested
        target branch there (an origin/ target is fetched first).
        """
        if not (work_run.repo_path and work_run.branch_name and work_run.base_branch):
            return None
        commit = await git.git_rev_parse(Path(work_run.repo_path), work_run.branch_name)
        if commit is None:
            return None
        from src.core.git import git_verify_commit_landed
        landed, _reason = await git_verify_commit_landed(
            Path(work_run.repo_path), work_run.base_branch, commit)
        if not landed:
            return None
        return work_run.base_branch, commit, work_run.repo_path

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
            work_run.repo_path, work_run.worktree_path, work_run.branch_name, work_run.id,
            Path(self._cfg.paths.worktree_dir),
            log_fields={"run_id": work_run.id, "session": session_id},
            label="Task worktree",
            fail_event="task_worktree_cleanup_failed",
            remove_failed_event="task_worktree_remove_failed",
        )

    async def _report_failure_to_parent(
        self, session_id: str, run: RunRecord, outcome: str, *, summary: str | None = None
    ) -> None:
        """Persist one failed/blocked child_report with stable source/recipient evidence.

        The source event is the Run's durable run_finished fact and the
        recipient is the close-time fixed parent, so the stable report id
        dedups across recovery and repeated finalize without ever relying on
        the legacy master_woke_after_summary judgment.
        """
        meta = await self._tree.load_meta(session_id)
        if meta is None or not meta.task_parent_id:
            return
        events = self._tree.runs.load_events_sync(session_id)
        source = next(
            (e for e in reversed(events)
             if e.get("type") == ET.RUN_FINISHED and e.get("run_id") == run.id), None)
        if source is None:
            return
        if summary is None:
            summary = await self._worker_failure_summary(session_id, run)
        await self._tree.dispatch.deliver_child_report(
            session_id,
            source_event=source,
            outcome=outcome,
            summary=summary,
            result_refs=[f"run:{run.id}"],
            recipient=meta.task_parent_id,
        )
        # The delivered failure report is the parent's new durable input: wake
        # its next serialized turn (dispatcher for a task-tree parent, the
        # legacy master wake for a legacy parent; deduped replays included).
        await self._tree.dispatch.wake_parent(meta.task_parent_id)


    async def _worker_failure_summary(self, session_id: str, run: RunRecord) -> str:
        """The failed run's own closing words (or the bare outcome) for the parent report."""
        events_log = self._tree.runs.run_dir(session_id, run.id) / "events.jsonl"
        if events_log.is_file():
            text = await asyncio.to_thread(review._worker_summary_from_events_log, events_log)
            if text:
                return text
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
            raise RuntimeError(
                f"TUI launch of run {self.run_id} created no tmux pane to pin a pid from")
        stat = read_pid_stat(pid)
        if stat is None:
            raise RuntimeError(
                f"TUI launch of run {self.run_id}: pane pid {pid} has no /proc entry; "
                "refusing to pin a start marker that cannot be verified")
        pid_start = stat[0]
        await self._tree.runs.record_launch(self.session_id, self.run_id, pid=pid, pid_start=pid_start)
        await self._tree.runs.record_observation(
            self.session_id, self.run_id, native_session_id=self.native_session_id)


async def fail_unlaunched_tui_run(
    tree: TaskTreeManager, session_id: str, run_id: str, *, reason: str,
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
        log.warning("tui_task_launch_marked_failed", session_id=session_id, run_id=run_id,
                    reason=reason)
    except Exception:
        log.error("tui_task_launch_failure_fact_failed", session_id=session_id, run_id=run_id,
                  reason=reason, exc_info=True)


async def prepare_tui_task_launch(
    cfg: CharlieBotConfig, session_id: str, tree: TaskTreeManager | None = None,
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
                raise TaskInvalidError(
                    f"task {session_id}: a tui terminal launch requires a manager node")
            owner_in_flight = session_id in _TUI_LAUNCH_INFLIGHT
            if not owner_in_flight:
                events = tree.runs.load_events_sync(session_id)
                runs = tree.runs.list_run_records_sync(session_id)
                if any(
                    r.kind == "manager_turn" and r.pid is not None and
                    not any(e.get("type") == ET.RUN_FINISHED and e.get("run_id") == r.id
                            for e in events)
                    for r in runs):
                    return None  # a live terminal Run already owns this task's terminal
                _TUI_LAUNCH_INFLIGHT.add(session_id)
                launch_no = 1 + len([r for r in runs if r.kind == "manager_turn"])
                run_id = stable_run_id(session_id, f"tui-launch:{launch_no}")
                record = RunRecord(
                    id=run_id, session_id=session_id, kind="manager_turn",
                    backend=meta.backend, model=option.model)
                await tree.runs.register_run_locked(
                    record, task_spec_text=canonical_task_spec_text(meta.task))
        if not owner_in_flight:
            break
        if time.monotonic() > deadline:
            raise TaskInvalidError(
                f"task {session_id}: another TUI terminal launch is still in flight")
        await asyncio.sleep(0.05)
    try:
        # Snapshot assembly and persistence run outside the control lock (they
        # take their own short holds); the bytes are committed before the tmux
        # session exists, so the launched claude reads the saved bytes.
        snapshot, _overlay_error, _declared = await assemble_coherent_snapshot(
            cfg, tree, meta, "manager_turn", option)
        snapshot_path = tree.runs.run_dir(session_id, run_id) / task_prompts.SNAPSHOT_FILENAME
        from src.core.json_utils import atomic_write_text
        await asyncio.to_thread(
            atomic_write_text, snapshot_path,
            json.dumps(snapshot.to_json_dict(), indent=2, ensure_ascii=False))
        await tree.runs.record_observation(session_id, run_id, prompt_snapshot_ref=str(snapshot_path))
        key = str(get_credentials().get("charliebot", "access_key") or "")
        if not key:
            raise RuntimeError(
                "run-token signing requires credentials.yaml charliebot.access_key; "
                "a TUI task launch cannot inject its run credential without it")
        token = sign_run_token(
            RunTokenClaims(session_id=session_id, run_id=run_id, agent=meta.name or "manager"), key)
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
