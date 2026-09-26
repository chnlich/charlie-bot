"""Completion evidence and the close / cancel / reopen transitions.

This module owns every evidence check and every blocker for manual
``complete``, automatic successful worker completion, operator ``cancel``,
and explicit ``reopen``. Nothing else appends a task_closed, task_reopened,
or task_close_requested fact.

Contracts this stage pins:

- No parent closes just because its children completed; every closure path
  re-checks unprocessed input (later arrivals included), active/queued/
  unresolved Runs, and open descendants against current facts.
- Valid completion is bound to this task's own evidence: run ids must be
  Runs of this session, structured result refs must resolve against this
  task's pinned specs / successful Runs / delivered review / landing, and a
  bare exit 0 or another task's success is never evidence. Slow evidence
  validation runs outside the control lock, followed by a locked
  revalidation of the relevant facts before the close fact lands.
- Operation ids are stable: the close/reopen event id derives from
  (session, request_id), so a duplicate request — including a retry after
  later close/reopen epochs — replays the original outcome instead of
  creating a second transition.
- A manager's own active Run may request its own closure: the request is
  durably saved with the VERIFIED caller's run id (never a spoofable payload
  field), the API answers 202 pending_run_finish, and re-evaluation happens
  only after that Run succeeds — replaying once per recovery pass, and
  leaving the task open with visible blockers when conditions changed.
- Agent permissions are own-node reporting and own-manager closure only;
  unauthorized mutation paths raise 403 and retries never bypass scope.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING

from src.core import event_types as ET
from src.core import session_dispatch
from src.core.control_events import (
    ACTOR_SYSTEM,
    ACTOR_USER,
    build_control_event,
    stable_close_event_id,
    stable_close_request_event_id,
    stable_input_ack_event_id,
    stable_reopen_event_id,
)
from src.core.log_once import LazyStructlogLogger
from src.core.models import SessionMetadata

if TYPE_CHECKING:
    from src.core.task_sessions import TaskTreeManager

log = LazyStructlogLogger()

# The structured result-ref grammar the evidence checks resolve against the
# task's own facts. Anything else is an opaque evidence pointer (a file path,
# a URL, a ticket id) carried as content — referenced, not proof.
RUN_REF_PREFIX = "run:"
SPEC_REF_PREFIX = "spec:"
REVIEW_REF_PREFIX = "review:"
LANDING_REF_PREFIX = "landed:"


@dataclass(frozen=True)
class LandingEvidence:
    """Target-branch landing proof one completion claim names.

    The execution-stage adapters supply the actual artifacts; this stage
    validates the claim against the task's frozen Run records (the landing
    branch must be a branch this task's runs were based on) and leaves the
    commit's merge verification to the adapters' own checks.
    """

    branch: str
    commit: str
    repo_path: str | None = None


@dataclass(frozen=True)
class CompletionEvidence:
    """The internal evidence bundle a completion claim carries.

    Manual complete builds it from the request body; the execution stage's
    automatic worker completion builds it from the run's recorded artifacts.
    ``review_run_ids`` and ``landing`` carry the implement workflow's review
    and target-branch delivery evidence.
    """

    summary: str = ""
    result_refs: list[str] = field(default_factory=list)
    run_ids: list[str] = field(default_factory=list)
    review_run_ids: list[str] = field(default_factory=list)
    landing: LandingEvidence | None = None


class TaskCompletionManager:
    """The close/cancel/reopen owner wired over one TaskTreeManager."""

    def __init__(self, tree: TaskTreeManager) -> None:
        self._tree = tree

    # ------------------------------------------------------------------
    # Blockers
    # ------------------------------------------------------------------

    def _execution_blockers(
        self,
        session_id: str,
        *,
        label: str,
        exclude_run_ids: set[str] | None = None,
    ) -> list[str]:
        """The Run blockers plus open-descendant blockers of one task, from current
        facts (lock held by caller). ``label`` names the caller in the
        caller-held-index error; ``exclude_run_ids`` carves out the Run whose
        own closure request is being re-evaluated.
        """
        tree = self._tree
        cached = tree._index
        if cached is None:
            raise RuntimeError(f"{label} blockers require the caller-held tree index")
        index = cached[0]
        tree._index_meta(index, session_id)  # 404 on an unknown task before any blocker text
        blockers: list[str] = []
        events = tree.runs.load_events_sync(session_id)
        for run in tree.runs.list_run_records_sync(session_id):
            if exclude_run_ids and run.id in exclude_run_ids:
                continue
            blocker = tree.runs.run_blocker(run, events, tree._host_boot_time())
            if blocker is not None:
                blockers.append(blocker)
        blockers.extend(
            f"has open descendant task {descendant}"
            for descendant in tree._descendants(index, session_id)
            if tree.task_state_of(index, descendant) == "open")
        return blockers

    def completion_blockers(
        self,
        session_id: str,
        *,
        exclude_run_ids: set[str] | None = None,
        exclude_input_ids: set[str] | None = None,
    ) -> list[str]:
        """The closure blockers of one task, from current facts (lock held by caller).

        Unprocessed input (including later arrivals), active/queued/unresolved
        Runs, and open descendants all block. ``exclude_run_ids`` carves out
        the Run whose own closure request is being re-evaluated;
        ``exclude_input_ids`` carves out exactly that Run's already-claimed
        batch — never later inputs or other Runs' claims.
        """
        blockers = self._execution_blockers(
            session_id, label="completion", exclude_run_ids=exclude_run_ids)
        pending = [
            e for e in self._tree.dispatch.pending_inputs(session_id)
            if str(e.get("id")) not in (exclude_input_ids or set())]
        if pending:
            blockers.insert(0, session_dispatch.unprocessed_input_blocker(pending))
        return blockers

    def cancellation_blockers(self, session_id: str) -> list[str]:
        """The cancellation blockers of one task, from current facts (lock held by caller).

        Explicit operator cancellation refuses active/unresolved execution and
        open descendants ONLY (plan 4.1: complete blocks on unprocessed input,
        cancel does not). Unprocessed input is preserved history on the
        cancelled node — refusing cancel over it would trap every task whose
        input nothing consumed yet.
        """
        return self._execution_blockers(session_id, label="cancellation")

    # ------------------------------------------------------------------
    # Evidence
    # ------------------------------------------------------------------

    def _delivery_run_outcomes(self, meta: SessionMetadata) -> tuple[dict[str, str | None], dict[str, str]]:
        """The delivery-run universe of one task: its own Runs plus the Runs of
        its direct children (a manager's delivery evidence legitimately cites
        the child work it consumed). Returns (run records by id, outcomes)."""
        tree = self._tree
        runs: dict[str, object] = {}
        outcomes: dict[str, str | None] = {}
        for run in tree.runs.list_run_records_sync(meta.id):
            runs[run.id] = run
            outcomes[run.id] = tree.facts_of(meta.id).run_outcomes.get(run.id)
        index = tree._index[0] if tree._index is not None else None
        if index is not None:
            for child_id in tree._children_of(index, meta.id):
                child_facts = tree.facts_of(child_id)
                for run in tree.runs.list_run_records_sync(child_id):
                    runs[run.id] = run
                    outcomes[run.id] = child_facts.run_outcomes.get(run.id)
        return runs, outcomes

    def evidence_blockers(self, meta: SessionMetadata, evidence: CompletionEvidence) -> list[str]:
        """Validate one completion claim against this task's own facts.

        Pure over the given facts snapshot; callers run it outside the control
        lock and revalidate the relevant facts/spec/refs under the lock
        before appending the close fact.
        """
        blockers: list[str] = []
        if not evidence.summary.strip():
            blockers.append("completion requires a non-empty summary")
        if not evidence.result_refs:
            blockers.append("completion requires result evidence (result_refs)")
        tree = self._tree
        runs, outcomes = self._delivery_run_outcomes(meta)
        facts = tree.facts_of(meta.id)
        claimed: dict[str, object] = {}
        for run_id in evidence.run_ids:
            run = runs.get(run_id)
            if run is None:
                blockers.append(f"run {run_id} is not a Run of task {meta.id} or its children")
                continue
            claimed[run_id] = run
            if outcomes.get(run_id) != "success":
                blockers.append(
                    f"run {run_id} has no successful run_finished fact"
                    + (" (a bare exit code is not evidence)" if run.exit_code == 0 else ""))
        if evidence.run_ids and not claimed:
            blockers.append("completion evidence names no delivery Run of this task")
        if not evidence.run_ids and any(
                outcome == "success" for outcome in outcomes.values()):
            # Delivery run evidence is required while the delivery universe has
            # a successful Run to cite.
            blockers.append("completion requires delivery run evidence (run_ids)")
        elif not evidence.run_ids and any(
                outcome == "failed" for outcome in outcomes.values()):
            # A failed-only subtree still refuses to be called completed on a
            # bare claim: the failure stays attention — cancel the failed work,
            # or retry it to success and cite that Run. Only a delivery
            # universe with neither a successful nor a failed Run (the
            # terminal-driven node's explicit operator completion;
            # cancelled-only or interrupted-only child work) closes on the
            # operator's own attributed evidence (summary + result_refs), and
            # every other closure blocker still applies.
            blockers.append(
                "completion requires delivery run evidence (run_ids); "
                "failed Runs are not completion evidence")
        for ref in evidence.result_refs:
            blockers.extend(self._structured_ref_blockers(meta, ref, runs, outcomes, facts, evidence))
        claimed_work_ids = set(evidence.run_ids)
        for review_run_id in evidence.review_run_ids:
            review = runs.get(review_run_id)
            if review is None or review.kind != "review" or review.session_id != meta.id:
                blockers.append(f"run {review_run_id} is not a review Run of task {meta.id}")
                continue
            if facts.run_outcomes.get(review_run_id) != "success":
                blockers.append(f"review run {review_run_id} has no successful run_finished fact")
                continue
            # The work/spec/review pin: a review authorizes only the work Run it
            # was chained to, so a historical successful review of a different
            # work attempt never completes this delivery.
            if getattr(review, "review_of_run_id", None) is not None:
                if review.review_of_run_id not in claimed_work_ids:
                    blockers.append(
                        f"review run {review_run_id} reviews work run {review.review_of_run_id}, "
                        "which this completion claim does not name")
            elif claimed_work_ids:
                blockers.append(
                    f"review run {review_run_id} is not chained to a work Run "
                    "(review_of_run_id is unset; it cannot prove which work it reviewed)")
        if meta.task is not None and meta.task.task_type is not None:
            task_type = str(meta.task.task_type.value)
        else:
            task_type = None
        if task_type == "implement":
            successful_reviews = [
                r for r in evidence.review_run_ids
                if r in runs and runs[r].kind == "review"
                and runs[r].session_id == meta.id and facts.run_outcomes.get(r) == "success"
                and (runs[r].review_of_run_id is None or runs[r].review_of_run_id in set(evidence.run_ids))]
            review_refs = [
                r for r in evidence.result_refs if r.startswith(REVIEW_REF_PREFIX)]
            if not successful_reviews and not review_refs:
                blockers.append(
                    "implement completion requires a successful review run of this task")
            if evidence.landing is None and not any(
                    r.startswith(LANDING_REF_PREFIX) for r in evidence.result_refs):
                blockers.append(
                    "implement completion requires target-branch landing evidence")
        return blockers

    def _structured_ref_blockers(
        self,
        meta: SessionMetadata,
        ref: str,
        runs: dict,
        outcomes: dict[str, str | None],
        facts: object,
        evidence: CompletionEvidence,
    ) -> list[str]:
        """Resolve one structured result ref against this task's facts.

        run:<id> must be a Run of THIS task; spec:<hash> must be a pinned task
        spec of one of the claimed runs; review:<id> a successful review Run
        of this task; landed:<branch>@<commit> a landing whose branch is a
        branch this task's runs were based on. Unknown shapes pass through as
        opaque evidence pointers.
        """

        blockers: list[str] = []
        if ref.startswith(RUN_REF_PREFIX):
            run_id = ref[len(RUN_REF_PREFIX):]
            if run_id not in runs:
                blockers.append(f"result ref {ref} does not name a Run of task {meta.id}")
        elif ref.startswith(SPEC_REF_PREFIX):
            spec_hash = ref[len(SPEC_REF_PREFIX):]
            claimed_ids = evidence.run_ids or list(runs)
            pinned = {runs[r].task_spec_hash for r in claimed_ids if r in runs}
            pinned.discard(None)
            if spec_hash not in pinned:
                blockers.append(
                    f"result ref {ref} does not match the task spec pinned by this task's runs")
        elif ref.startswith(REVIEW_REF_PREFIX):
            review_id = ref[len(REVIEW_REF_PREFIX):]
            review = runs.get(review_id)
            if review is None or review.kind != "review" or review.session_id != meta.id:
                blockers.append(f"result ref {ref} does not name a review Run of task {meta.id}")
            elif facts.run_outcomes.get(review_id) != "success":  # type: ignore[union-attr]
                blockers.append(f"result ref {ref} names a review without a successful run_finished fact")
        elif ref.startswith(LANDING_REF_PREFIX):
            body = ref[len(LANDING_REF_PREFIX):]
            branch, _, commit = body.partition("@")
            if not branch or not commit:
                blockers.append(f"result ref {ref} must be landed:<branch>@<commit>")
            else:
                base_branches = {
                    runs[r].base_branch for r in runs if r in runs and runs[r].base_branch}
                if meta.task is not None and meta.task.base_branch:
                    base_branches.add(meta.task.base_branch)
                if base_branches and branch not in base_branches:
                    blockers.append(
                        f"result ref {ref} lands on {branch}; this task's runs target "
                        f"{', '.join(sorted(base_branches))}")
        return blockers

    # ------------------------------------------------------------------
    # Landing verification (the git truth behind a landed: claim)
    # ------------------------------------------------------------------

    def _landing_claims(self, meta: SessionMetadata, evidence: CompletionEvidence) -> list[tuple[str, str, str | None]]:
        """Every (branch, commit, repo_path override) landing claim one completion carries.

        The structured ``landed:<branch>@<commit>`` result refs and the
        adapter's structured ``landing`` field are the two forms; both pass the
        same git verification, so a forged ref cannot buy what a structured
        bundle cannot.
        """
        claims: list[tuple[str, str, str | None]] = []
        if evidence.landing is not None:
            claims.append(
                (evidence.landing.branch, evidence.landing.commit, evidence.landing.repo_path))
        for ref in evidence.result_refs:
            if not ref.startswith(LANDING_REF_PREFIX):
                continue
            body = ref[len(LANDING_REF_PREFIX):]
            branch, _, commit = body.partition("@")
            if branch and commit:
                claims.append((branch, commit, None))
        return claims

    async def landing_blockers(self, meta: SessionMetadata, evidence: CompletionEvidence) -> list[str]:
        """Verify every landing claim against the requested repository and target branch.

        Existence and ancestry run through the existing git helpers OUTSIDE the
        control lock (subprocess work never holds it). A claim that cannot be
        proven — fake commit, real-but-unmerged commit, wrong branch or repo,
        unreadable repository — is an explicit blocker; nothing defaults to
        verified.
        """
        blockers: list[str] = []
        from src.core.git import git_verify_commit_landed

        claims = self._landing_claims(meta, evidence)
        if not claims:
            return blockers
        fallback_repo = meta.task.repo_path if meta.task is not None else None
        if fallback_repo is None:
            for run in self._tree.runs.list_run_records_sync(meta.id):
                if run.repo_path:
                    fallback_repo = run.repo_path
                    break
        for branch, commit, repo_override in claims:
            repo = repo_override or fallback_repo
            if not repo:
                blockers.append(
                    f"landing {branch}@{commit} cannot be verified: no repository is recorded "
                    "for this task or its runs")
                continue
            try:
                landed, reason = await git_verify_commit_landed(Path(repo), branch, commit)
            except OSError as e:
                blockers.append(f"landing {branch}@{commit} verification failed in {repo}: {e}")
                continue
            if not landed:
                blockers.append(f"landing evidence unverified in {repo}: {branch}@{commit}: {reason}")
        return blockers

    async def verified_evidence_blockers(self, meta: SessionMetadata, evidence: CompletionEvidence) -> list[str]:
        """The full evidence check: the shape/record layer plus the git landing layer.

        The slow git verification runs outside the control lock; the locked
        revalidation re-runs the shape layer over fresh facts (a landed commit
        cannot un-land, so the outside-lock git verdict stands).
        """
        blockers = self.evidence_blockers(meta, evidence)
        blockers.extend(await self.landing_blockers(meta, evidence))
        return blockers

    # ------------------------------------------------------------------
    # Complete
    # ------------------------------------------------------------------

    async def complete_task(
        self,
        session_id: str,
        *,
        request_id: str,
        evidence: CompletionEvidence,
        caller: object,
    ) -> tuple[int, dict]:
        """One completion operation: 200 Session, 202 pending_run_finish, or 409 blockers.

        Operator callers close immediately (after evidence validation and a
        locked revalidation). A run-token agent may request closure only for
        its own manager task, only through its own active Run: the request is
        saved durably with the verified owner run id and re-evaluated once
        that Run finishes successfully.
        """
        from src.core.run_token import CallerIdentity
        from src.core.task_sessions import (
            TaskConflictError,
            TaskForbiddenError,
            TaskInvalidError,
        )

        if not request_id:
            raise TaskInvalidError("request_id is required for completion")
        tree = self._tree
        caller_run_id: str | None = None
        if isinstance(caller, CallerIdentity) and not caller.is_operator:
            claims = caller.claims
            assert claims is not None
            if claims.session_id != session_id:
                raise TaskForbiddenError("an agent may only request closure of its own task")
            meta = await tree.load_task_meta(session_id)
            if meta.profile != "manager":
                raise TaskForbiddenError("own-run closure requests come from manager tasks only")
            run = await tree.runs.get_run(session_id, claims.run_id)
            if run is None:
                raise TaskInvalidError(f"caller run {claims.run_id} not found in task {session_id}")
            events = tree.runs.load_events_sync(session_id)
            if tree.runs.run_has_terminal_fact(run, events) or not tree.runs.run_is_active(
                    run, events, tree._host_boot_time()):
                raise TaskConflictError(
                    [f"caller run {claims.run_id} is not active; it cannot request closure"])
            caller_run_id = claims.run_id
            replay = self._replay_close_request(session_id, request_id)
            if replay is not None:
                return replay
            return await self._save_close_request(
                session_id, request_id=request_id, owner_run_id=caller_run_id, evidence=evidence)

        replay = self._replay_close_request(session_id, request_id)
        if replay is not None:
            return replay
        return await self._close_now(
            session_id, request_id=request_id, evidence=evidence, actor=ACTOR_USER)

    def _replay_close_request(self, session_id: str, request_id: str) -> tuple[int, dict] | None:
        """The original outcome of an already-recorded operation id, if one exists.

        Covers both the pending close request (202 replay) and the landed
        close (200 replay) — including retries arriving after later
        close/reopen epochs, which return the ORIGINAL outcome without a new
        transition.
        """
        tree = self._tree
        events = tree.fact_history(session_id)
        for event in events:
            if event.get("type") == ET.TASK_CLOSE_REQUESTED and event.get("request_id") == request_id:
                return 202, {
                    "session_id": session_id,
                    "request_id": request_id,
                    "status": "pending_run_finish",
                }
        for event in events:
            if event.get("type") == ET.TASK_CLOSED and event.get("request_id") == request_id:
                return 200, {"session_id": session_id, "closed_event_id": event.get("id")}
        return None

    async def _save_close_request(
        self,
        session_id: str,
        *,
        request_id: str,
        owner_run_id: str,
        evidence: CompletionEvidence,
    ) -> tuple[int, dict]:
        """Durably save one own-run closure request and answer 202 (lock held inside)."""
        tree = self._tree
        async with tree.control_lock:
            replay = self._replay_close_request(session_id, request_id)
            if replay is not None:
                return replay
            event = build_control_event(
                ET.TASK_CLOSE_REQUESTED,
                actor=ACTOR_SYSTEM,
                source_session_id=session_id,
                event_id=stable_close_request_event_id(session_id, request_id),
                request_id=request_id,
                owner_run_id=owner_run_id,
                summary=evidence.summary,
                result_refs=list(evidence.result_refs),
                run_ids=list(evidence.run_ids),
            )
            await tree.events.append(session_id, event)
        log.info("task_close_requested", session_id=session_id, request_id=request_id,
                 owner_run_id=owner_run_id)
        return 202, {"session_id": session_id, "request_id": request_id, "status": "pending_run_finish"}

    async def _close_now(
        self,
        session_id: str,
        *,
        request_id: str,
        evidence: CompletionEvidence,
        actor: str,
        exclude_run_ids: set[str] | None = None,
        exclude_input_ids: set[str] | None = None,
    ) -> tuple[int, dict]:
        """Evaluate, validate, and land one completed close (the operator path).

        Facts are snapshotted and checked under the lock, evidence validation
        runs outside it, and the relevant facts/spec/refs are revalidated
        under the lock before the close fact lands.
        """
        from src.core.task_sessions import TaskConflictError

        tree = self._tree
        async with tree.control_lock:
            index = await tree._get_index()
            tree._index_meta(index, session_id)
            blockers = self.completion_blockers(
                session_id, exclude_run_ids=exclude_run_ids, exclude_input_ids=exclude_input_ids)
            meta = await tree.load_meta(session_id)
            assert meta is not None
        if blockers:
            raise TaskConflictError(sorted(set(blockers)))
        # The potentially slow evidence validation — including the git landing
        # verification — runs outside the control lock and is authoritative:
        # an unproven claim (forged hash, unlanded commit, wrong branch or
        # repo) aborts the close here. The locked pass below revalidates the
        # fast facts (structure plus the shape layer) — a verified landing
        # cannot un-land between the two passes, and a moving condition
        # (run finished, input processed) is caught by the fresh structural
        # and shape revalidation.
        evidence_blockers = await self.verified_evidence_blockers(meta, evidence)
        if evidence_blockers:
            raise TaskConflictError(sorted(set(evidence_blockers)))
        # Live-announce epochs are taken before any append, so the announce
        # feed can never double-render an event the aggregator caught up on.
        child_epoch = await tree.sessions.prime_aggregator(session_id)
        parent_epoch = (await tree.sessions.prime_aggregator(meta.task_parent_id)
                        if meta.task_parent_id else None)
        async with tree.control_lock:
            index = await tree._get_index()
            tree._index_meta(index, session_id)
            if tree.task_state_of(index, session_id) != "open":
                raise TaskConflictError([f"task {session_id} is no longer open"])
            fresh_blockers = self.completion_blockers(
                session_id, exclude_run_ids=exclude_run_ids, exclude_input_ids=exclude_input_ids)
            fresh_meta = await tree.load_meta(session_id)
            assert fresh_meta is not None
            fresh_evidence_blockers = self.evidence_blockers(fresh_meta, evidence)
            all_blockers = sorted(set(fresh_blockers + fresh_evidence_blockers))
            if all_blockers:
                # The locked revalidation is authoritative: conditions changed
                # during the outside-the-lock validation leave the task open
                # with the current, visible blockers.
                raise TaskConflictError(all_blockers)
            close_event, report, report_created = await self._append_closed(
                session_id, fresh_meta, request_id=request_id, evidence=evidence, actor=actor)
        await tree.sessions.announce_appended_event(session_id, close_event, epoch=child_epoch)
        if report_created and parent_epoch is not None:
            await tree.sessions.announce_appended_event(
                str(fresh_meta.task_parent_id), report, epoch=parent_epoch)
        if report_created and fresh_meta.task_parent_id:
            # The delivered report is the parent's new durable input: its next
            # serialized turn wakes now (dispatcher for a task-tree parent,
            # the legacy master wake for a legacy parent). The close fact and
            # the report are already durable at this point, so a failed wake
            # must not fail the close: the automatic-completion callers would
            # classify the landed boundary as blocked and deliver a
            # contradictory second report over the completed close. The wake
            # is separately re-drivable — any later dispatch or recovery pass
            # launches the parent's queued reservation with the delivered
            # report — so the failure is logged loudly and the close result
            # stands.
            try:
                await tree.dispatch.wake_parent(str(fresh_meta.task_parent_id))
            except Exception as exc:
                log.warning("close_parent_wake_failed", session_id=session_id,
                            parent=str(fresh_meta.task_parent_id), report=str(report.get("id")),
                            error=str(exc))
        return 200, {"session_id": session_id, "closed_event_id": close_event["id"]}

    async def _append_closed(
        self,
        session_id: str,
        meta: SessionMetadata,
        *,
        request_id: str,
        evidence: CompletionEvidence,
        actor: str,
    ) -> dict:
        """Append task_closed with its fixed parent recipient, then deliver the report.

        Lock held by the caller. report_to fixes this closure's delivery
        ownership at close time; retries, recovery, and reparenting can never
        retarget it. The report lands before the lock releases (a control
        fact); the live announcements follow outside it (the caller's).
        """
        tree = self._tree
        close_event = build_control_event(
            ET.TASK_CLOSED,
            actor=actor,
            source_session_id=session_id,
            event_id=stable_close_event_id(session_id, request_id),
            request_id=request_id,
            outcome="completed",
            summary=evidence.summary,
            result_refs=list(evidence.result_refs),
            run_ids=list(evidence.run_ids),
            report_to=meta.task_parent_id,
        )
        await tree.events.append(session_id, close_event)
        report, report_created = await tree.dispatch.deliver_child_report_locked(
            session_id,
            source_event=close_event,
            outcome="completed",
            summary=evidence.summary,
            result_refs=list(evidence.result_refs),
            recipient=meta.task_parent_id,
            actor=ACTOR_SYSTEM,
        )
        self._tree._invalidate_index()
        return close_event, report, report_created

    # ------------------------------------------------------------------
    # Own-run close re-evaluation and automatic worker completion
    # ------------------------------------------------------------------

    async def recheck_close_requests(self, session_id: str, run_id: str) -> list[str]:
        """Re-evaluate one Run's pending closure requests after it finished successfully.

        Recovery replays the same request once (the stable request id dedups);
        changed conditions leave the task open with the blockers visible.
        Returns the blockers that kept each request open (empty when a close
        landed or nothing was pending).
        """
        tree = self._tree
        from src.core.task_sessions import TaskConflictError

        facts = tree.facts_of(session_id)
        outcomes = facts.run_outcomes
        if outcomes.get(run_id) != "success":
            return []
        remaining: list[str] = []
        for request in facts.close_requests:
            if request.get("owner_run_id") != run_id:
                continue
            request_id = str(request.get("request_id") or "")
            if not request_id:
                continue
            if self._replay_close_request(session_id, request_id) is not None:
                replay = self._replay_close_request(session_id, request_id)
                assert replay is not None
                if replay[0] == 200:
                    continue  # already closed by an earlier pass
            evidence = CompletionEvidence(
                summary=str(request.get("summary") or ""),
                result_refs=list(request.get("result_refs") or []),
                run_ids=list(request.get("run_ids") or []),
            )
            # The owner Run's already-claimed batch is excluded from the
            # pending blockers; its success acknowledged exactly that batch.
            run = await tree.runs.get_run(session_id, run_id)
            claimed = set(run.input_event_ids) if run is not None else set()
            try:
                await self._close_now(
                    session_id, request_id=request_id, evidence=evidence, actor=ACTOR_SYSTEM,
                    exclude_run_ids={run_id}, exclude_input_ids=claimed)
            except TaskConflictError as e:
                # Changed conditions leave the task open with visible blockers.
                blockers = list(getattr(e, "blockers", None) or [])
                remaining.extend(blockers)
                log.info("task_close_request_still_blocked", session_id=session_id,
                         request_id=request_id, blockers=blockers)
        return remaining

    async def evaluate_automatic_completion(
        self,
        session_id: str,
        *,
        run_id: str,
        summary: str | None = None,
        result_refs: list[str] | None = None,
        request_id: str | None = None,
        evidence: CompletionEvidence | None = None,
    ) -> tuple[int, dict]:
        """Automatic successful worker completion: finish first, then close checks.

        The current Run must already be durably finished (the dispatcher's
        finish path guarantees the ordering) and its outcome must be a
        success — failed or interrupted evidence keeps the task open. Evidence
        defaults derive from the successful Run record itself (its pinned
        spec, its result ref); the execution-stage adapters pass the recorded
        artifacts explicitly. A caller-supplied *evidence* bundle — the
        implement workflow's review/landing delivery — replaces the work-only
        default wholesale and passes the same verified close checks.
        """
        from src.core.task_sessions import TaskConflictError, TaskForbiddenError, TaskNotFoundError

        tree = self._tree
        meta = await tree.load_task_meta(session_id)
        if meta.profile != "worker":
            raise TaskForbiddenError("automatic completion applies to worker tasks")
        facts = tree.facts_of(session_id)
        if facts.run_outcomes.get(run_id) != "success":
            raise TaskConflictError(
                [(f"run {run_id} has no successful run_finished fact; "
                  "failed or interrupted evidence keeps the task open")])
        run = await tree.runs.get_run(session_id, run_id)
        if run is None:
            from src.core.task_sessions import TaskNotFoundError
            raise TaskNotFoundError(f"run {run_id} not found in task {session_id}")
        if evidence is not None:
            effective_evidence = evidence
        else:
            refs = list(result_refs if result_refs is not None else [f"{RUN_REF_PREFIX}{run_id}"])
            if run.task_spec_hash:
                refs.append(f"{SPEC_REF_PREFIX}{run.task_spec_hash}")
            effective_evidence = CompletionEvidence(
                summary=summary or f"Run {run_id} completed",
                result_refs=refs,
                run_ids=[run_id],
            )
        evidence = effective_evidence
        effective_request_id = request_id or f"auto:{run_id}"
        replay = self._replay_close_request(session_id, effective_request_id)
        if replay is not None:
            return replay
        return await self._close_now(
            session_id, request_id=effective_request_id, evidence=evidence, actor=ACTOR_SYSTEM)

    async def after_run_finished(self, session_id: str, run_id: str) -> None:
        """The one post-success follow-up owner, driven by the durable outcome.

        A successful Run re-evaluates its manager's pending close requests. A
        successful worker WORK Run evaluates automatic completion unless the
        task's delivery rule waits for more evidence — an implement task's
        delivery waits for its review and target-branch landing, which the
        execution adapter drives through :meth:`evaluate_automatic_completion`
        with the full verified bundle. A blocked automatic close keeps its
        blockers visible and the adapters re-evaluate.
        """
        from src.core.task_sessions import TaskConflictError

        tree = self._tree
        meta = await tree.load_task_meta(session_id)
        await self.recheck_close_requests(session_id, run_id)
        facts = tree.facts_of(session_id)
        if facts.run_outcomes.get(run_id) != "success":
            return
        run = await tree.runs.get_run(session_id, run_id)
        if run is None or meta.profile != "worker" or run.kind != "work":
            return
        task_type = str(meta.task.task_type.value) if (meta.task is not None and meta.task.task_type) else None
        if task_type == "implement":
            log.info("worker_delivery_awaits_review", session_id=session_id, run_id=run_id)
            return
        try:
            await self.evaluate_automatic_completion(session_id, run_id=run_id)
        except TaskConflictError as e:
            log.info("automatic_worker_completion_blocked",
                     session_id=session_id, run_id=run_id, blockers=getattr(e, "blockers", None))

    # ------------------------------------------------------------------
    # Input acknowledgement (the terminal-driven node's explicit resolution)
    # ------------------------------------------------------------------

    async def acknowledge_inputs(
        self,
        session_id: str,
        *,
        request_id: str,
        input_ids: list[str],
        note: str,
        caller: object,
    ) -> dict:
        """The operator's durable confirmation that exact task inputs were handled.

        The terminal-driven node takes input through its terminal; the durable
        input facts it handled out-of-band stay pending until this explicit
        resolution names them. Guards (all shared with the ordinary closure
        path): operator credentials only — a run-token agent can never confirm
        on behalf of the operator; each id must be an exact currently-pending
        input of this task (later arrivals and other Runs' claims are not
        acknowledgable, and unknown ids refuse); the fact is durable,
        attributable (actor=user, request id, note), and idempotent — a
        replayed request id returns the original acknowledgement and
        re-acking an already-acknowledged id is a no-op. Nothing here
        completes anything: closure still runs the ordinary guards, so
        active Runs, open children, and any input that arrived after the
        acknowledgement keep blocking.
        """
        from src.core.run_token import CallerIdentity
        from src.core.task_sessions import (
            TaskConflictError,
            TaskForbiddenError,
            TaskInvalidError,
        )

        if not isinstance(caller, CallerIdentity) or not caller.is_operator:
            raise TaskForbiddenError("acknowledging task input requires operator credentials")
        if not request_id:
            raise TaskInvalidError("request_id is required for input acknowledgement")
        if not input_ids:
            raise TaskInvalidError("input_ids is required and must name at least one input")
        if len(set(input_ids)) != len(input_ids):
            raise TaskInvalidError("input_ids must not repeat an id")
        tree = self._tree
        replay = self._replay_input_ack(session_id, request_id)
        if replay is not None:
            return replay
        epoch = await tree.sessions.prime_aggregator(session_id)
        async with tree.control_lock:
            index = await tree._get_index()
            # 404 on an unknown task before any acknowledgement text.
            tree._index_meta(index, session_id)
            replay = self._replay_input_ack(session_id, request_id)
            if replay is not None:
                return replay
            pending = {str(e.get("id")): e for e in tree.dispatch.pending_inputs(session_id)}
            acknowledged = self._acknowledged_input_ids(session_id)
            unknown = [i for i in input_ids if i not in pending and i not in acknowledged]
            if unknown:
                raise TaskConflictError(
                    [session_dispatch.inputs_not_pending_conflict(session_id, unknown)])
            acked_now = [i for i in input_ids if i in pending]
            if not acked_now:
                # Every named id was already acknowledged: the replay of that
                # resolution is the same no-op, not a second fact.
                return {
                    "session_id": session_id,
                    "acknowledged_event_id": None,
                    "input_ids": [],
                    "already_acknowledged": list(input_ids),
                }
            event = build_control_event(
                ET.TASK_INPUT_ACKNOWLEDGED,
                actor=ACTOR_USER,
                source_session_id=session_id,
                event_id=stable_input_ack_event_id(session_id, request_id),
                request_id=request_id,
                input_ids=acked_now,
                note=note,
            )
            await tree.events.append(session_id, event)
            tree._invalidate_index()
        await tree.sessions.announce_appended_event(session_id, event, epoch=epoch)
        return {
            "session_id": session_id,
            "acknowledged_event_id": str(event["id"]),
            "input_ids": acked_now,
            "already_acknowledged": [i for i in input_ids if i not in acked_now],
        }

    def _acknowledged_input_ids(self, session_id: str) -> set[str]:
        """The input ids this task's facts already acknowledge."""
        tree = self._tree
        facts = tree.facts_of(session_id)
        return set(facts.confirmed_input_ids)

    def _replay_input_ack(self, session_id: str, request_id: str) -> dict | None:
        """The original outcome of an already-recorded acknowledgement id."""
        tree = self._tree
        event_id = stable_input_ack_event_id(session_id, request_id)
        for event in tree.fact_history(session_id):
            if (event.get("type") == ET.TASK_INPUT_ACKNOWLEDGED
                    and event.get("id") == event_id):
                return {
                    "session_id": session_id,
                    "acknowledged_event_id": event_id,
                    "input_ids": list(event.get("input_ids") or []),
                    "already_acknowledged": [],
                }
        return None

    # ------------------------------------------------------------------
    # Cancel and reopen
    # ------------------------------------------------------------------

    async def cancel_task(
        self,
        session_id: str,
        *,
        request_id: str,
        reason: str,
        caller: object,
    ) -> dict:
        """Explicit operator cancellation with reason, preserving all evidence.

        Refuses active/unresolved execution or open children with 409 — never
        unprocessed input, which stays as preserved history on the cancelled
        node. It never recursively stops the subtree (Run cancel stays the
        separate operation), and duplicate request ids replay the original
        outcome.
        """
        from src.core.run_token import CallerIdentity
        from src.core.task_sessions import TaskConflictError, TaskForbiddenError, TaskInvalidError

        if not isinstance(caller, CallerIdentity) or not caller.is_operator:
            raise TaskForbiddenError("task cancellation requires operator credentials")
        if not request_id:
            raise TaskInvalidError("request_id is required for cancellation")
        if not reason.strip():
            raise TaskInvalidError("cancellation requires a reason")
        tree = self._tree
        replay = self._replay_close_request(session_id, request_id)
        if replay is not None:
            return replay[1]
        pre_meta = await tree.load_task_meta(session_id)
        child_epoch = await tree.sessions.prime_aggregator(session_id)
        parent_epoch = (await tree.sessions.prime_aggregator(pre_meta.task_parent_id)
                        if pre_meta.task_parent_id else None)
        async with tree.control_lock:
            index = await tree._get_index()
            meta = tree._index_meta(index, session_id)
            if tree.task_state_of(index, session_id) != "open":
                raise TaskConflictError(
                    [(f"task {session_id} is {tree.task_state_of(index, session_id)}; "
                      "only an open task can be cancelled")])
            replay = self._replay_close_request(session_id, request_id)
            if replay is not None:
                return replay[1]
            blockers = self.cancellation_blockers(session_id)
            if blockers:
                raise TaskConflictError(sorted(set(blockers)))
            close_event = build_control_event(
                ET.TASK_CLOSED,
                actor=ACTOR_USER,
                source_session_id=session_id,
                event_id=stable_close_event_id(session_id, request_id),
                request_id=request_id,
                outcome="cancelled",
                summary=reason,
                result_refs=[],
                run_ids=[],
                report_to=meta.task_parent_id,
            )
            await tree.events.append(session_id, close_event)
            report, report_created = await tree.dispatch.deliver_child_report_locked(
                session_id,
                source_event=close_event,
                outcome="cancelled",
                summary=reason,
                result_refs=[],
                recipient=meta.task_parent_id,
                actor=ACTOR_SYSTEM,
            )
            tree._invalidate_index()
        await tree.sessions.announce_appended_event(session_id, close_event, epoch=child_epoch)
        if report_created and parent_epoch is not None:
            await tree.sessions.announce_appended_event(
                str(meta.task_parent_id), report, epoch=parent_epoch)
        if report_created and meta.task_parent_id:
            # Same delivered-report wake the completed close performs. The
            # cancelled close and its report are already durable here, so a
            # failed wake is logged and the cancellation result stands (the
            # parent's turn is re-drivable; see _close_now).
            try:
                await tree.dispatch.wake_parent(str(meta.task_parent_id))
            except Exception as exc:
                log.warning("cancel_parent_wake_failed", session_id=session_id,
                            parent=str(meta.task_parent_id), report=str(report.get("id")),
                            error=str(exc))
        return {"session_id": session_id, "closed_event_id": close_event["id"]}

    async def reopen_task(
        self,
        session_id: str,
        *,
        request_id: str,
        reason: str,
        caller: object,
        closed_event_id: str | None = None,
    ) -> dict:
        """Explicit operator reopen of one closed task.

        References the relevant closed event (the latest close by default),
        refuses closed ancestors with their list, preserves history, never
        touches automation_paused or authorization, and does not reactivate
        earlier already-handled history. Duplicate operation ids — including
        retries after later close/reopen events — replay the original outcome.
        """
        from src.core.run_token import CallerIdentity
        from src.core.task_sessions import (
            TaskConflictError,
            TaskForbiddenError,
            TaskInvalidError,
        )

        if not isinstance(caller, CallerIdentity) or not caller.is_operator:
            raise TaskForbiddenError("task reopen requires operator credentials")
        if not request_id:
            raise TaskInvalidError("request_id is required for reopen")
        tree = self._tree
        events = tree.fact_history(session_id)
        for event in events:
            if event.get("type") == ET.TASK_REOPENED and event.get("request_id") == request_id:
                return {"session_id": session_id, "reopened_event_id": event.get("id")}
        # The live-announce epoch is taken before the append, like the close
        # paths: the fact lands under the lock, the notification follows it.
        child_epoch = await tree.sessions.prime_aggregator(session_id)
        async with tree.control_lock:
            index = await tree._get_index()
            meta = tree._index_meta(index, session_id)
            facts = tree.facts_of(session_id)
            if closed_event_id is not None:
                close = next(
                    (c for c in facts.close_events if c.get("id") == closed_event_id), None)
                if close is None:
                    raise TaskInvalidError(
                        f"event {closed_event_id} is not a close fact of task {session_id}")
            else:
                if not facts.close_events:
                    raise TaskInvalidError(f"task {session_id} is not closed")
                close = facts.close_events[-1]
            replay = self._replay_close_request(session_id, request_id)
            if replay is not None and replay[0] == 200:
                # A close landed while this reopen waited on the lock.
                raise TaskConflictError(
                    [f"task {session_id} was closed again during the reopen; use a fresh request"])
            for event in tree.fact_history(session_id):
                if event.get("type") == ET.TASK_REOPENED and event.get("request_id") == request_id:
                    return {"session_id": session_id, "reopened_event_id": event.get("id")}
            await tree._require_open_ancestry_from_index(index, session_id)
            _ = meta
            reopen_event = build_control_event(
                ET.TASK_REOPENED,
                actor=ACTOR_USER,
                source_session_id=session_id,
                event_id=stable_reopen_event_id(session_id, request_id),
                request_id=request_id,
                closed_event_id=str(close.get("id")),
                reason=reason,
            )
            await tree.events.append(session_id, reopen_event)
            tree._invalidate_index()
        await tree.sessions.announce_appended_event(session_id, reopen_event, epoch=child_epoch)
        return {"session_id": session_id, "reopened_event_id": reopen_event["id"]}
