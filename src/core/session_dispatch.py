"""Input delivery, deduplication, claims, and parent reports for the task tree.

This module owns the input side of the delivery stage: admission of browser,
agent-relay, cron, and child-report input as durable facts with stable
identity; the pure recoverable pending-input calculation over the fact
history; the per-node input claims that bind one exact batch to one Run
before launch; and the parent-report delivery with its fixed recipient.

Ordering contract (the one this stage exists to pin):

- Every accepted input is durable (``chat_events.jsonl``, through the control
  sink, under the tree control write lock) before any acknowledgement, live
  notification, or launch effect. Live notification happens after the lock is
  released; a notification failure is repaired by catch-up, never by a second
  persisted copy.
- A node has at most one executing input consumer while siblings progress
  independently: a Run claims one exact batch, later arrivals stay pending for
  the next run, and only that Run's own successful ``run_finished``
  acknowledges its batch. Failures and interruptions acknowledge nothing.
- Recovery derives everything from disk facts — the valid input boundary
  (post-creation, or post-import plus the explicitly listed old pending
  inputs), minus successful acknowledgements, minus live claims — so a fresh
  manager computes the same pending set as the process that died.
"""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

from src.core import event_types as ET
from src.core.control_events import (
    ACTOR_AGENT,
    ACTOR_SYSTEM,
    ACTOR_USER,
    build_control_event,
    stable_child_report_id,
)
from src.core.log_once import LazyStructlogLogger
from src.core.models import RunRecord

if TYPE_CHECKING:
    from src.core.task_sessions import TaskTreeManager

log = LazyStructlogLogger()

# The event types that carry consumable task input. Real user input is the
# USER type and nothing else: agent relays, scheduled triggers, and child
# reports keep their own types even when their text contains a takeoff
# phrase, so machine input can never mint or revoke a user authorization
# window (the takeoff gate judges ET.USER events only).
INPUT_EVENT_TYPES: frozenset[str] = frozenset(
    {ET.USER, ET.AGENT_MESSAGE, ET.SCHEDULED_TRIGGER, ET.CHILD_REPORT})


def child_report_text(report: dict) -> str:
    """A child_report event as the parent's turn input: the typed header, then its summary."""
    return (f"[Report from task {report.get('child_session_id')} | "
            f"outcome {report.get('outcome')}] {str(report.get('summary') or '')}")


# The admitted input types a message route may produce. A run-token caller on
# the user-message route is agent input; only verified operator credentials
# are user input (see input_event_type_for_caller).
ROUTE_INPUT_TYPES: frozenset[str] = frozenset({ET.USER, ET.AGENT_MESSAGE})


def unprocessed_input_blocker(pending: list[dict]) -> str:
    """The blocker sentence for a pending-input list: first 8 ids, then (+N more)."""
    ids = ", ".join(str(e.get("id")) for e in pending[:8])
    more = "" if len(pending) <= 8 else f" (+{len(pending) - 8} more)"
    return f"has unprocessed input: {ids}{more}"


def inputs_not_pending_conflict(session_id: str, unknown: list[str]) -> str:
    """The 409 conflict sentence for ack/claim ids outside the pending set."""
    return f"input(s) not pending for {session_id}: {', '.join(unknown)}"


class TaskInputDispatcher:
    """The input/report owner wired over one TaskTreeManager."""

    def __init__(self, tree: TaskTreeManager) -> None:
        self._tree = tree
        # The execution-stage seam: an async callable (session_id, pending input
        # event dicts) that binds a Run and launches. None until the execution
        # adapters land — admission and recovery work without it, and no
        # acknowledgement ever stands in for a Run that did not run.
        self.executor: object | None = None

    # ------------------------------------------------------------------
    # Admission
    # ------------------------------------------------------------------

    async def admit_input(
        self,
        session_id: str,
        *,
        event_type: str,
        content: str,
        actor: str,
        uploaded_files: list[dict] | None = None,
        input_id: str | None = None,
        timestamp: str | None = None,
        from_session: str | None = None,
        from_session_name: str | None = None,
    ) -> dict:
        """Persist one input as a durable fact and return it.

        Identity is stable: a caller retrying with the same ``input_id`` gets
        the original event back and no second append. The event lands before
        the lock is released; the live announcement follows outside the lock.
        ``event_type`` is the caller's proof of origin — the user-message
        route may mint USER only for operator callers (the route enforces
        that; this method refuses the mismatch as a backstop).
        """
        from src.core.task_sessions import TaskForbiddenError, TaskInvalidError

        if event_type not in INPUT_EVENT_TYPES:
            raise TaskInvalidError(f"{event_type} is not a task input type")
        if event_type == ET.USER and actor != ACTOR_USER:
            raise TaskForbiddenError("only verified operator input is a real user message")
        if event_type == ET.SCHEDULED_TRIGGER and actor != ACTOR_SYSTEM:
            # A scheduled input's provenance is the server's own fire — the
            # scheduler derives the identity and stamps actor=system. No
            # caller (run-token agent included) can mint one.
            raise TaskForbiddenError("only the server's own scheduler mints scheduled triggers")
        tree = self._tree
        epoch = await tree.sessions.prime_aggregator(session_id)
        async with tree.control_lock:
            await tree.load_task_meta(session_id)
            events = tree.fact_history(session_id)
            if input_id is not None:
                existing = next((e for e in events if e.get("id") == input_id), None)
                if existing is not None:
                    # A replayed input keeps its original identity, timestamps,
                    # content, and attachments; no second durable copy exists.
                    return existing
            event = build_control_event(
                event_type,
                actor=actor,
                source_session_id=session_id,
                event_id=input_id,
                content=content,
            )
            if timestamp is not None:
                event["timestamp"] = timestamp  # imported history keeps original times
            if uploaded_files:
                event["uploaded_files"] = uploaded_files
            if from_session is not None:
                event["from_session"] = from_session
            if from_session_name is not None:
                event["from_session_name"] = from_session_name
            await tree.events.append(session_id, event)
        await tree.sessions.announce_appended_event(session_id, event, epoch=epoch)
        return event

    # ------------------------------------------------------------------
    # Pending inputs (pure, recoverable)
    # ------------------------------------------------------------------

    def pending_inputs(self, session_id: str) -> list[dict]:
        """The task's currently pending input events, derived from disk facts.

        Exactly the valid input boundary (post-creation for fresh tasks;
        post-import plus only the explicitly listed old pending inputs for
        imported ones), minus ids a successful run_finished acknowledged,
        minus ids a launched Run holds. A Run that launched (its record has a
        pid, or the log holds its run_finished fact) keeps its batch claimed
        permanently — success, failed, interrupted, or stopped: the round ran
        and its outcome is the handling, so its batch never reappears as
        pending. A Run that never launched and was stop-requested claims
        nothing: it will never run, so its batch returns to the pending set
        for the next consumer. A queued (registered, never launched) Run
        still claims, as now.
        """
        from src.core.task_sessions import TaskInvalidError

        tree = self._tree
        facts = tree.facts_of(session_id)
        if facts.boundary_index is None and not facts.imported_pending_ids:
            raise TaskInvalidError(
                f"task {session_id} has no creation or import boundary; it is not a task-tree node")
        runs = tree.runs.list_run_records_sync(session_id)
        events = tree.fact_history(session_id)
        # Everything the facts fold treats as acknowledged: a successful run's
        # batch, and the operator's durable input acknowledgements.
        confirmed: set[str] = set(facts.confirmed_input_ids)
        claimed: set[str] = set()
        for run in runs:
            launched = run.pid is not None or tree.runs.run_has_terminal_fact(run, events)
            if not launched and tree.runs.stop_requested(events, run.id):
                continue  # a never-launched, stop-requested run releases its batch
            claimed.update(run.input_event_ids)
        pending: dict[str, dict] = {}
        for event in facts.input_candidates:
            input_id = str(event.get("id"))
            if input_id in confirmed or input_id in claimed:
                continue
            pending[input_id] = event
        return list(pending.values())

    def pending_input_blockers(self, session_id: str) -> list[str]:
        """The structural-mutation blocker form of the pending set (the tree seam)."""
        pending = self.pending_inputs(session_id)
        if not pending:
            return []
        return [unprocessed_input_blocker(pending)]

    # ------------------------------------------------------------------
    # Claims
    # ------------------------------------------------------------------

    async def claim_input_batch(self, session_id: str, run_id: str, *, input_ids: list[str] | None = None) -> list[str]:
        """Atomically bind the exact pending batch to *run_id* before launch.

        Later arrivals stay pending for the next run. The binding is the run
        record's own metadata write under the control lock, so a concurrent
        consumer can never double-claim, and a finished or already-launched or
        stop-requested run claims nothing.
        """
        async with self._tree.control_lock:
            return await self.claim_input_batch_locked(session_id, run_id, input_ids=input_ids)

    async def claim_input_batch_locked(
        self, session_id: str, run_id: str, *, input_ids: list[str] | None = None
    ) -> list[str]:
        """The claim's core, for callers already holding the control lock.

        The executor's reservation binds the batch inside its own lock hold;
        taking the lock again here would deadlock a non-reentrant asyncio.Lock.
        """
        from src.core.json_utils import atomic_write_text
        from src.core.task_sessions import TaskConflictError, TaskNotFoundError

        tree = self._tree
        run = await tree.runs.get_run(session_id, run_id)
        if run is None:
            raise TaskNotFoundError(f"run {run_id} not found in session {session_id}")
        events = tree.runs.load_events_sync(session_id)
        if tree.runs.run_has_terminal_fact(run, events):
            raise TaskConflictError([f"run {run_id} already finished; it claims no new inputs"])
        if run.pid is not None:
            raise TaskConflictError([f"run {run_id} already launched; it owns its bound batch"])
        if tree.runs.stop_requested(events, run_id):
            raise TaskConflictError([f"run {run_id} has a durable stop request; it never launches"])
        pending_ids = [str(e.get("id")) for e in self.pending_inputs(session_id)]
        if input_ids is None:
            batch = pending_ids
        else:
            unknown = [i for i in input_ids if i not in pending_ids]
            if unknown:
                raise TaskConflictError(
                    [inputs_not_pending_conflict(session_id, unknown)])
            batch = list(input_ids)
        run.input_event_ids = [*run.input_event_ids, *batch]
        await asyncio.to_thread(
            atomic_write_text,
            tree.runs.metadata_path(session_id, run_id),
            run.model_dump_json(indent=2),
        )
        return batch

    # ------------------------------------------------------------------
    # Launch decision (the executor seam)
    # ------------------------------------------------------------------

    async def dispatch_pending(self, session_id: str) -> dict:
        """Evaluate the launch decision for one node's pending inputs.

        Closed nodes keep late input as history; paused
        nodes keep it durable without starting work. An active consumer owns
        the node: later arrivals wait for the next serialized run. A queued
        (registered, never launched) run is a pending execution request — an
        explicit retry, or a run a crashed process registered — and is handed
        to the executor to launch rather than deadlocking the node on itself;
        a stop-requested queued run never launches and releases its batch. A
        past failure never blocks fresh dispatch: a launched round's batch is
        handled whatever its outcome, and redoing work is the operator's
        re-send or retry. With an executor registered the pending batch
        is handed over after admission; without one this stage records exactly
        that and acknowledges nothing.
        """

        tree = self._tree
        meta = await tree.load_task_meta(session_id)
        pending = self.pending_inputs(session_id)
        decision: dict = {"session_id": session_id, "pending": len(pending)}
        if tree.task_state(session_id) != "open":
            decision["launch"] = False
            decision["reason"] = "task is closed; input retained as history"
            return decision
        if meta.automation_paused:
            decision["launch"] = False
            decision["reason"] = "automation_paused; input retained until resume"
            return decision
        tui_refusal = self._tui_manager_refusal(meta)
        if tui_refusal is not None:
            # A tui-cli manager node takes input through the terminal, not a
            # headless SDK turn. No Run is reserved and none fails: the input
            # stays durable and pending, the decision names the transport
            # limit, and the terminal remains the node's execution surface.
            decision["launch"] = False
            decision["reason"] = tui_refusal
            return decision
        events = tree.runs.load_events_sync(session_id)
        queued: list[RunRecord] = []
        for run in tree.runs.list_run_records_sync(session_id):
            if tree.runs.run_has_terminal_fact(run, events):
                continue
            if run.pid is not None:
                decision["launch"] = False
                decision["reason"] = f"run {run.id} already consumes this node's inputs"
                return decision
            if tree.runs.stop_requested(events, run.id):
                continue  # a stopped queued run is never launched
            if run.kind in ("iteration", "scheduled_step"):
                # Sequence Runs are their controller's launches: they need the
                # controller's composed prompt and sequence context, so the
                # dispatcher never starts one headlessly (and an interrupted
                # improve iteration is never silently auto-resumed). The
                # owning controller or the recovery re-drive advances them.
                continue
            if run.kind == "manager_turn" and not run.input_event_ids and not pending:
                # A void reservation (registered, never claimed): with nothing
                # pending it must never launch an empty side-effecting turn,
                # and it must not block the node either — skip it like a
                # stopped run. A later input hands it the pending batch again
                # (the deterministic repair) and it becomes the consumer.
                continue
            queued.append(run)
        if queued:
            if self.executor is None:
                decision["launch"] = False
                decision["reason"] = "execution adapters register in the next stage"
                log.info("task_input_executor_pending", session_id=session_id, pending=len(pending))
                return decision
            launched = await self.executor(  # type: ignore[misc]
                session_id, pending, launch_run_id=queued[0].id)
            if launched is None:
                # A concurrent dispatch launched it first, or the durable facts
                # (terminal fact, stop request) refused it; nothing was started.
                decision["launch"] = False
                decision["reason"] = f"queued run {queued[0].id} was not launched by this call"
                return decision
            decision["launch"] = True
            decision["run_id"] = launched
            return decision
        if not pending:
            decision["launch"] = False
            decision["reason"] = "no pending inputs"
            return decision
        if self.executor is None:
            decision["launch"] = False
            decision["reason"] = "execution adapters register in the next stage"
            log.info("task_input_executor_pending", session_id=session_id, pending=len(pending))
            return decision
        launched = await self.executor(session_id, pending)  # type: ignore[misc]
        if launched is None:
            # A concurrent dispatch won the reservation; this call scheduled
            # no process and the batch stays claimed by the winner's Run.
            decision["launch"] = False
            decision["reason"] = "another dispatch reserved this batch"
            return decision
        decision["launch"] = True
        decision["run_id"] = launched
        return decision

    async def wake_parent(
        self, parent_id: str, *, report: dict, caller_session_id: str | None = None
    ) -> asyncio.Task | None:
        """The one parent-wake entry after a report delivery.

        A legacy parent whose own turn closed the task (caller_session_id equal
        to the parent id) skips the wake: the parent's own turn already holds
        the outcome in its HTTP response. The report argument is the
        child_report the caller just delivered — the legacy wake renders it
        through child_report_text as compose_input_prompt renders it, and the
        event appended by deliver_child_report_locked stays the durable record;
        a task-tree parent's next serialized turn dispatches from its durable
        inputs and never consults the caller.

        The legacy wake is a whole master turn (minutes on a slow backend), so
        it is scheduled, never awaited: the startup reconcile pass that replays
        a lost report must not hold the server's doors shut for the turn, and a
        close request must not wait on it. Returns that scheduled task, or None
        when the wake was skipped or the parent is missing.
        """
        tree = self._tree
        meta = await tree.load_meta(parent_id)
        if meta is None:
            log.warning("wake_parent_target_missing", parent_id=parent_id)
            return None
        if meta.profile is not None:
            await self.dispatch_pending(parent_id)
            return None
        if caller_session_id == parent_id:
            log.info("legacy_parent_wake_skipped", parent=parent_id,
                     report=report.get("id"), caller=caller_session_id)
            return None
        text = child_report_text(report)
        from src.core.master_trigger import trigger_master
        from src.core.tasks import create_logged_task

        return create_logged_task(trigger_master(parent_id, text, tree._cfg, tree.sessions),
                                  name=f"legacy-parent-wake-{parent_id[:8]}")

    def _tui_manager_refusal(self, meta) -> str | None:
        """Why a headless manager turn must not start on *meta*, or None.

        Only a tui-cli-backed MANAGER node refuses: its turns are the user's
        terminal session (tmux takes input through the browser terminal, not
        the SDK), so a dispatched input can never be executed headlessly.
        Worker nodes keep their ordinary adapters; a tui worker is a launch
        failure, not a terminal-driven node.
        """
        from src.core.backend_models import BackendType

        if meta.profile != "manager" or not meta.backend:
            return None
        option = self._tree._cfg.get_backend_option(meta.backend)
        if option is None or option.type is not BackendType.TUI_CLI:
            return None
        return (
            "tui-cli manager takes input through the terminal; no headless "
            "manager turn is started and the input stays pending")

    async def finish_run(
        self,
        session_id: str,
        run_id: str,
        *,
        outcome: str,
        exit_code: int | None = None,
        ended_at: object | None = None,
        input_event_ids: list[str] | None = None,
    ) -> object:
        """The one entry adapters use to land a Run's terminal fact.

        The acknowledgement payload is validated against the Run's registered
        batch (a failure or interruption acknowledges nothing), the fact lands
        first, and only afterwards do the follow-ups run: a successful Run
        re-evaluates its own manager's pending close requests, and a
        successful worker work Run evaluates automatic completion. Those
        re-evaluations re-acquire the control lock themselves — this method
        never holds it across them.
        """
        from datetime import datetime

        from src.core.task_sessions import TaskConflictError

        tree = self._tree
        if input_event_ids:
            # A run without a registered batch may acknowledge only currently
            # pending inputs of this node (the master-turn shape): ids bound to
            # another Run's batch or already acknowledged are foreign, and a
            # foreign id never lands as a terminal fact.
            run_for_check = await tree.runs.get_run(session_id, run_id)
            if run_for_check is not None and not run_for_check.input_event_ids:
                pending_ids = {str(e.get("id")) for e in self.pending_inputs(session_id)}
                foreign = [i for i in input_event_ids if i not in pending_ids]
                if foreign:
                    raise TaskConflictError(
                        [inputs_not_pending_conflict(session_id, foreign)])
        async with tree.control_lock:
            run = await tree.runs.record_finish_locked(
                session_id, run_id, outcome,
                input_event_ids=input_event_ids, exit_code=exit_code,
                ended_at=ended_at if isinstance(ended_at, datetime) or ended_at is None else None)
        # The terminal fact is durable: a worker node's busy interval (its
        # header timer) closes with the Run that opened it. Best-effort: a
        # notification failure is logged by the run owner's seam and never
        # fails the finish.
        await tree.runs.notify_liveness(session_id, run, launched=False)
        # First terminal fact wins: the durable outcome — never the outcome
        # argument a losing concurrent finisher passed — governs every
        # post-finish action. The completion owner owns the follow-up policy
        # (close-request rechecks, automatic worker completion); a blocked
        # automatic close keeps its blockers visible and the adapters
        # re-evaluate.
        durable = await tree.runs.terminal_outcome_of(session_id, run_id)
        if durable == "success":
            try:
                await tree.completion.after_run_finished(session_id, run_id)
            except TaskConflictError as e:
                log.info("post_finish_completion_blocked",
                         session_id=session_id, run_id=run_id, blockers=getattr(e, "blockers", None))
        return run

    # ------------------------------------------------------------------
    # Parent reports
    # ------------------------------------------------------------------

    def report_source_event(self, child_session_id: str, child_kind: str) -> dict:
        """The child's latest durable run fact: the source event one report rides on.

        The report id derives from the source event's id, so repeated
        finalization and recovery passes must pick the same fact: the latest
        ``run_finished`` over the full fact history, or the child's creation
        fact when no run finished. A child with no durable fact at all fails
        loudly here — a report without a source event has no stable id to
        dedup on.
        """
        events = self._tree.fact_history(child_session_id)
        source = next((e for e in reversed(events)
                       if e.get("type") in (ET.RUN_FINISHED, ET.TASK_CREATED)), None)
        if source is None:
            raise RuntimeError(
                f"{child_kind} {child_session_id} has no durable fact to source its report from")
        return source

    async def deliver_child_report(
        self,
        child_session_id: str,
        *,
        source_event: dict,
        outcome: str,
        summary: str,
        result_refs: list[str] | None = None,
        recipient: str | None,
        actor: str = ACTOR_AGENT,
    ) -> dict | None:
        """Persist one child_report fact to the fixed recipient's log.

        The report id derives from the child event and the recipient, so a
        retry, a recovery pass, or a reparent re-derives the same id and finds
        the already-delivered report instead of duplicating it. Parent log
        persistence IS delivery — the parent model consuming it is a separate,
        later concern. A root task (recipient None) requires no receipt.
        """

        if recipient is None:
            return None
        tree = self._tree
        epoch = await tree.sessions.prime_aggregator(recipient)
        async with tree.control_lock:
            report, created = await self.deliver_child_report_locked(
                child_session_id, source_event=source_event, outcome=outcome, summary=summary,
                result_refs=result_refs, recipient=recipient, actor=actor)
        if created:
            await tree.sessions.announce_appended_event(recipient, report, epoch=epoch)
        return report

    async def deliver_child_report_locked(
        self,
        child_session_id: str,
        *,
        source_event: dict,
        outcome: str,
        summary: str,
        result_refs: list[str] | None = None,
        recipient: str | None,
        actor: str = ACTOR_AGENT,
    ) -> tuple[dict, bool]:
        """deliver_child_report for a caller already holding the control lock.

        Returns (report, created): created is False when the stable report id
        already exists in the recipient's fact history (the dedup path), so the
        caller never announces a second copy of a delivered report.
        """
        from src.core.task_sessions import TaskNotFoundError

        if recipient is None:
            return {}, False
        tree = self._tree
        report_id = stable_child_report_id(
            child_session_id, str(source_event.get("id")), recipient)
        parent_meta = await tree.load_meta(recipient)
        if parent_meta is None:
            raise TaskNotFoundError(f"report recipient task {recipient} not found")
        parent_events = tree.fact_history(recipient)
        existing = next((e for e in parent_events if e.get("id") == report_id), None)
        if existing is not None:
            return existing, False
        report = build_control_event(
            ET.CHILD_REPORT,
            actor=actor,
            source_session_id=child_session_id,
            event_id=report_id,
            child_session_id=child_session_id,
            child_event_id=str(source_event.get("id")),
            outcome=outcome,
            summary=summary,
            result_refs=list(result_refs or []),
        )
        await tree.events.append(recipient, report)
        return report, True

    async def recover_pending_reports(self, session_id: str) -> list[dict]:
        """Repair the crash window *child result saved before parent append*.

        Scans this task's close facts whose ``report_to`` names a recipient and
        delivers every close report the recipient's log does not hold yet.
        Recovery never duplicates: the stable report id dedups against the
        parent's fact history, so repeated passes and fresh instances converge
        on the same single report event. Late reports to a closed parent stay
        history (the parent is not reopened by them).
        """
        tree = self._tree
        await tree.load_task_meta(session_id)
        facts = tree.facts_of(session_id)
        delivered: list[dict] = []
        for close in facts.close_events:
            recipient = close.get("report_to")
            if not recipient:
                continue
            report_id = stable_child_report_id(session_id, str(close.get("id")), str(recipient))
            parent_events = tree.fact_history(str(recipient))
            if any(e.get("id") == report_id for e in parent_events):
                continue  # already delivered: a repeat pass never re-reports it
            outcome = str(close.get("outcome") or "completed")
            report = await self.deliver_child_report(
                session_id,
                source_event=close,
                outcome=outcome,
                summary=str(close.get("summary") or ""),
                result_refs=list(close.get("result_refs") or []),
                recipient=str(recipient),
                actor=ACTOR_SYSTEM,
            )
            if report is not None:
                delivered.append(report)
        return delivered

    def undelivered_report_blockers(self, session_id: str) -> list[str]:
        """The reparent-guard form of the crash-window repair: every close fact
        this task still owes its fixed recipient (the entire moving subtree is
        guarded by the caller walking its nodes)."""
        tree = self._tree
        facts = tree.facts_of(session_id)
        blockers: list[str] = []
        for close in facts.close_events:
            recipient = close.get("report_to")
            if not recipient:
                continue
            report_id = stable_child_report_id(session_id, str(close.get("id")), str(recipient))
            parent_events = tree.fact_history(str(recipient))
            if not any(e.get("id") == report_id for e in parent_events):
                blockers.append(
                    f"has an undelivered parent report for close event {close.get('id')}")
        return blockers


def input_event_type_for_caller(caller: object) -> str:
    """The input type a verified caller identity may produce on a message route.

    Browser and operator credentials are user input; a run-token agent on the
    same route stays agent input with its own session's provenance — it can
    never manufacture a real USER event or another caller's provenance.
    """
    from src.core.run_token import CallerIdentity
    from src.core.task_sessions import TaskForbiddenError

    if isinstance(caller, CallerIdentity):
        if caller.is_operator:
            return ET.USER
        claims = caller.claims
        assert claims is not None
        return ET.AGENT_MESSAGE
    assert ET.USER in ROUTE_INPUT_TYPES and ET.AGENT_MESSAGE in ROUTE_INPUT_TYPES
    raise TaskForbiddenError("message input requires verified caller credentials")


def agent_provenance(caller: object) -> tuple[str | None, str | None]:
    """The (from_session, from_session_name) provenance for agent-relayed input."""
    from src.core.run_token import CallerIdentity

    if isinstance(caller, CallerIdentity) and not caller.is_operator:
        claims = caller.claims
        assert claims is not None
        return claims.session_id, None
    return None, None
