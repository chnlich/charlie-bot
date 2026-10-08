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
from collections.abc import Iterator
from typing import TYPE_CHECKING, Any, Protocol

from src.infra import event_types as ET
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import RunRecord, SessionMetadata, SessionStatus, ensure_utc
from src.runtime.control_events import (
    ACTOR_AGENT,
    ACTOR_SYSTEM,
    ACTOR_USER,
    build_control_event,
    stable_child_report_id,
)
from src.runtime.task_errors import (
    TaskArchivedError,
    TaskConflictError,
    TaskForbiddenError,
    TaskInvalidError,
    TaskNotFoundError,
)

if TYPE_CHECKING:
  from src.runtime.control_sink import ControlEventSink
  from src.runtime.runs import RunStore
  from src.runtime.sessions import SessionManager

log = LazyStructlogLogger()


class DispatchFacts(Protocol):
  """The folded task facts the dispatcher reads; ``task_sessions`` owns the fold."""

  boundary_index: int | None
  imported_pending_ids: frozenset[str]
  confirmed_input_ids: set[str]
  input_candidates: list[dict]
  close_events: list[dict]


class DispatchCompletion(Protocol):
  """The completion owner members the dispatcher calls."""

  async def restore_chain_locked(self, session_id: str, *, request_id: str,
                                 reason: str) -> tuple[list[str], list[tuple[str, dict, int]]]:
    ...

  async def after_run_finished(self, session_id: str, run_id: str) -> None:
    ...


class DispatchTree(Protocol):
  """The task tree members the dispatcher calls; ``TaskTreeManager`` implements them.

  ``index`` is the tree's rebuildable index: the dispatcher hands it back to the
  tree and never reads it, so its type is ``Any``.
  """

  control_lock: asyncio.Lock
  events: ControlEventSink
  runs: RunStore
  completion: DispatchCompletion

  @property
  def sessions(self) -> SessionManager:
    ...

  async def load_meta(self, session_id: str) -> SessionMetadata | None:
    ...

  async def load_task_meta(self, session_id: str) -> SessionMetadata:
    ...

  def task_state(self, session_id: str) -> str:
    ...

  def fact_history(self, session_id: str) -> list[dict]:
    ...

  def facts_of(self, session_id: str) -> DispatchFacts:
    ...

  async def _get_index(self) -> Any:
    ...

  def _index_meta(self, index: Any, session_id: str) -> SessionMetadata:
    ...

  def _ancestors(self, index: Any, session_id: str) -> list[SessionMetadata]:
    ...

  def archived_of(self, index: Any, meta: SessionMetadata) -> bool:
    ...

  def invalidate_tree_index(self) -> None:
    ...

  async def check_task_authorization(self, session_id: str) -> str:
    ...


# The event types that carry consumable task input. Real user input is the
# USER type and nothing else: agent relays, scheduled triggers, and child
# reports keep their own types even when their text contains a takeoff
# phrase, so machine input can never mint or revoke a user authorization
# window (the takeoff gate judges ET.USER events only).
INPUT_EVENT_TYPES: frozenset[str] = frozenset({ET.USER, ET.AGENT_MESSAGE, ET.SCHEDULED_TRIGGER, ET.CHILD_REPORT})


def child_report_text(report: dict) -> str:
  """A child_report event as the parent's turn input: the typed header, then its summary."""
  return (
      f"[Report from task {report.get('child_session_id')} | "
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

  def __init__(self, tree: DispatchTree) -> None:
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
    epoch = await tree.session_events.prime_aggregator(session_id)
    restore_announcements: list[tuple[str, dict, int]] = []
    async with tree.control_lock:
      meta = await tree.load_task_meta(session_id)
      if tree.task_state(session_id) != "open" and event_type != ET.USER:
        # The archived node answers machine input with the one archived
        # sentence, whatever the sender's own standing — the user's message is
        # the only input that gets past this gate (it restores just below).
        raise TaskArchivedError(session_id)
      await self._authorize_agent_message(meta, event_type, from_session)
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
      if tree.task_state(session_id) != "open":
        # A real user message is the one input that restores an archived
        # chain: the target and every archived ancestor reopen (topmost
        # first), then this message lands past the fresh boundary as the
        # round's only input. The restore's request id is the input event's
        # id, so a replayed message with a stable input id re-derives the
        # same restore facts instead of duplicating them.
        _restored, restore_announcements = await tree.completion.restore_chain_locked(
            session_id, request_id=str(event["id"]), reason="user message")
      await tree.events.append(session_id, event)
      if event_type == ET.USER:
        # A real user message is the one input that reorders the
        # sidebar: it lifts the node and its unarchived ancestors to
        # the event's time (the replay return above lifts nothing, and
        # the other input types are agent/server traffic). The climb
        # stops before the first archived ancestor — the archived row
        # and everything above it keep their places, since above an
        # archived parent the node is already Workspace's own root.
        # Each write rides update_thinking_state's one-field fresh
        # mutate, so a concurrent edit survives and the listing
        # revision advances; the lift only ever moves forward.
        index = await tree._get_index()
        when = ensure_utc(event["timestamp"])
        chain = [tree._index_meta(index, session_id)]
        for ancestor in tree._ancestors(index, session_id):
          if ancestor.status == SessionStatus.ARCHIVED or tree.archived_of(index, ancestor):
            break
          chain.append(ancestor)
        for node in chain:
          current = await tree.sessions.store.get_session(node.id)
          if when > current.updated_at:
            await tree.sessions.update_thinking_state(node.id, when)
        tree.invalidate_tree_index()
    # The restore's per-node announcements ride the same after-lock window as
    # restore_chain's: each reopened fact reaches the page from its own node
    # (the rows leave the archived list), then the message itself.
    for node_id, reopen_event, reopen_epoch in restore_announcements:
      await tree.session_events.announce_appended_event(node_id, reopen_event, epoch=reopen_epoch)
    await tree.session_events.announce_appended_event(session_id, event, epoch=epoch)
    return event

  async def _authorize_agent_message(self, meta, event_type: str, from_session: str | None) -> None:
    """The request-entry gate an agent message to a worker node passes.

        The one judgment `_authorize_agent_creation` applies when the worker is
        created, re-applied where a new instruction enters the tree: the sender
        must be the worker's own parent, and the parent must hold the
        nearest-real-user-ancestor authorization (takeoff_gate) for
        implementation work. The read-only verify exemption rides the same
        one shared judgment (takeoff_gate.is_verify_exempt). A refusal raises
        TaskForbiddenError before anything is enqueued; every entry that
        delivers an agent message to a worker node passes through here.
        """
    from src.runtime.takeoff_gate import DelegationBlockedError, is_verify_exempt
    if event_type != ET.AGENT_MESSAGE or meta.profile != "worker":
      return
    if is_verify_exempt(meta.task):
      return
    if from_session != meta.task_parent_id:
      raise TaskForbiddenError(
          f"agent messages to worker task {meta.id} must come from its parent task "
          f"{meta.task_parent_id}; this message names sender {from_session}")
    try:
      await self._tree.check_task_authorization(from_session or "")
    except DelegationBlockedError as e:
      raise TaskForbiddenError(str(e)) from e

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
    tree = self._tree
    facts = tree.facts_of(session_id)
    if facts.boundary_index is None and not facts.imported_pending_ids:
      raise TaskInvalidError(f"task {session_id} has no creation or import boundary; it is not a task-tree node")
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

  async def claim_input_batch_locked(self,
                                     session_id: str,
                                     run_id: str,
                                     *,
                                     input_ids: list[str] | None = None) -> list[str]:
    """The claim's core, for callers already holding the control lock.

        The executor's reservation binds the batch inside its own lock hold;
        taking the lock again here would deadlock a non-reentrant asyncio.Lock.
        """
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
        raise TaskConflictError([inputs_not_pending_conflict(session_id, unknown)])
      batch = list(input_ids)
    run.input_event_ids = [*run.input_event_ids, *batch]
    await tree.runs.write_record(session_id, run)
    return batch

  # ------------------------------------------------------------------
  # Launch decision (the executor seam)
  # ------------------------------------------------------------------

  async def dispatch_pending(self, session_id: str) -> dict:
    """Evaluate the launch decision for one node's pending inputs.

        Closed nodes keep late input as history. An active consumer owns
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
    await tree.load_task_meta(session_id)  # raises for an absent or non-task session
    pending = self.pending_inputs(session_id)
    decision: dict = {"session_id": session_id, "pending": len(pending)}
    if tree.task_state(session_id) != "open":
      decision["launch"] = False
      decision["reason"] = "task is closed; input retained as history"
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
      self, parent_id: str, *, report: dict, caller_session_id: str | None = None) -> asyncio.Task | None:
    """Dispatch a newly delivered child report to its parent task.

        The dispatch reads the parent's durable inputs and never consults the
        caller — not even the parent's own turn closing its child: a busy node
        defers the launch to its turn-end dispatch, a closed node retains the
        report as history, and the delivered report event stays the durable
        record either way. Returns None: the dispatch is synchronous with the
        caller; no scheduled master wake exists to return.
        """
    await self.dispatch_pending(parent_id)
    return None

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
          raise TaskConflictError([inputs_not_pending_conflict(session_id, foreign)])
    async with tree.control_lock:
      run = await tree.runs.record_finish_locked(
          session_id,
          run_id,
          outcome,
          input_event_ids=input_event_ids,
          exit_code=exit_code,
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
        log.info(
            "post_finish_completion_blocked",
            session_id=session_id,
            run_id=run_id,
            blockers=getattr(e, "blockers", None))
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
    source = next((e for e in reversed(events) if e.get("type") in (ET.RUN_FINISHED, ET.TASK_CREATED)), None)
    if source is None:
      raise RuntimeError(f"{child_kind} {child_session_id} has no durable fact to source its report from")
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
  ) -> tuple[dict, bool]:
    """Persist one child_report fact to the fixed recipient's log, then wake the parent.

        The report id derives from the child event and the recipient, so a
        retry, a recovery pass, or a reparent re-derives the same id and finds
        the already-delivered report instead of duplicating it. Parent log
        persistence IS delivery — the parent model consuming it is a separate,
        later concern. A root task (recipient None) requires no receipt.

        Returns (report, created) exactly like deliver_child_report_locked:
        created is True only when this call appended the report, and a None
        recipient returns (None, False). Only a freshly created report is the
        parent's new durable input, so a wake decision reads created, never
        the report's presence.

        This entry owns the wake: after the lock is released, a created=True
        delivery wakes the recipient exactly once (wake_parent), so callers
        never add their own. A replayed delivery (created False) wakes nobody.
        The locked entry stays wake-free: its callers write further events
        under the lock and wake themselves after releasing it.
        """

    if recipient is None:
      return None, False
    tree = self._tree
    epoch = await tree.session_events.prime_aggregator(recipient)
    async with tree.control_lock:
      report, created = await self.deliver_child_report_locked(
          child_session_id,
          source_event=source_event,
          outcome=outcome,
          summary=summary,
          result_refs=result_refs,
          recipient=recipient,
          actor=actor)
    if created:
      await tree.session_events.announce_appended_event(recipient, report, epoch=epoch)
      if tree.task_state(recipient) != "open":
        # The archived parent keeps the report as history: it counts as
        # delivered and never wakes the node.
        log.info(
            "child_report_into_archived_parent_kept_as_history",
            parent=recipient,
            child=child_session_id,
            report=str(report.get("id")),
        )
        return report, created
      await self.wake_parent(recipient, report=report)
    return report, created

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
    if recipient is None:
      return {}, False
    tree = self._tree
    parent_meta = await tree.load_meta(recipient)
    if parent_meta is None:
      raise TaskNotFoundError(f"report recipient task {recipient} not found")
    report_id, existing = self._child_report_delivery(child_session_id, str(source_event.get("id")), recipient)
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

  def _child_report_delivery(self, child_session_id: str, source_event_id: str,
                             recipient: str) -> tuple[str, dict | None]:
    """The pair's stable report id and the fact already delivered under it, or None.

        One home for report-delivery identity. The id derives from (child
        session, source event, recipient) via ``stable_child_report_id``; a
        report counts as delivered exactly when that id sits in the
        recipient's full fact history. The delivery path, the recovery scan,
        and the reparent guard read this, so the three cannot disagree about
        which reports are still owed.
        """
    report_id = stable_child_report_id(child_session_id, source_event_id, recipient)
    delivered = next((e for e in self._tree.fact_history(recipient) if e.get("id") == report_id), None)
    return report_id, delivered

  def _undelivered_close_recipients(self, session_id: str) -> Iterator[tuple[dict, str]]:
    """Yield (close fact, recipient) for each close of *session_id* whose report the
        recipient's fact history does not hold yet. The recovery scan and the reparent
        guard consume this one walk, so they cannot disagree about which close reports
        are still owed."""
    facts = self._tree.facts_of(session_id)
    for close in facts.close_events:
      recipient = close.get("report_to")
      if not recipient:
        continue
      _report_id, already = self._child_report_delivery(session_id, str(close.get("id")), str(recipient))
      if already is None:
        yield close, str(recipient)

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
    delivered: list[dict] = []
    for close, recipient in self._undelivered_close_recipients(session_id):
      outcome = str(close.get("outcome") or "completed")
      report, created = await self.deliver_child_report(
          session_id,
          source_event=close,
          outcome=outcome,
          summary=str(close.get("summary") or ""),
          result_refs=list(close.get("result_refs") or []),
          recipient=recipient,
          actor=ACTOR_SYSTEM,
      )
      if created:
        delivered.append(report)
    return delivered

  def undelivered_report_blockers(self, session_id: str) -> list[str]:
    """The reparent-guard form of the crash-window repair: every close fact
        this task still owes its fixed recipient (the entire moving subtree is
        guarded by the caller walking its nodes)."""
    return [
        f"has an undelivered parent report for close event {close.get('id')}"
        for close, _ in self._undelivered_close_recipients(session_id)
    ]


def input_event_type_for_caller(caller: object) -> str:
  """The input type a verified caller identity may produce on a message route.

  Browser and operator credentials are user input; a run-token agent on the
  same route stays agent input with its own session's provenance — it can
  never manufacture a real USER event or another caller's provenance.
  """
  from src.runtime.run_token import CallerIdentity

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
  from src.runtime.run_token import CallerIdentity

  if isinstance(caller, CallerIdentity) and not caller.is_operator:
    claims = caller.claims
    assert claims is not None
    return claims.session_id, None
  return None, None
