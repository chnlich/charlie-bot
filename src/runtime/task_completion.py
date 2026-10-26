"""Completion evidence and the archive / cancel / reopen transitions.

This module owns every evidence check and every blocker for manual
``archive``, automatic successful worker completion, operator ``cancel``,
explicit ``reopen``, and the sequence controllers' close of an ended loop's
worker child. Nothing else appends a task_closed, task_reopened,
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
- A manager's own active Run may request its own closure: a request that
  already faces blockers — everything except what exists only because that
  Run is still active — is refused 409 with them and saves nothing. Otherwise
  the request is durably saved with the VERIFIED caller's run id (never a
  spoofable payload field), the API answers 202 pending_run_finish, and
  re-evaluation happens only after that Run succeeds — replaying once per
  recovery pass. Conditions that changed meanwhile leave the task open and
  deliver the blockers to the requesting session as one wake input per
  request.
- Agent permissions are own-node reporting, own-manager closure, and
  cancellation of a direct child of the agent's own task; unauthorized
  mutation paths raise 403 and retries never bypass scope.
- A sequence controller closes its ended loop's worker child through
  ``close_sequence_child``: the lifecycle outcome stays "completed" (proven
  delivery) or "cancelled" (every other ending), and the loop's own outcome
  rides the close fact as ``sequence_outcome`` and becomes the delivered
  report's outcome.
"""

from __future__ import annotations

import dataclasses
import pathlib
import uuid
from typing import TYPE_CHECKING, Any, Literal, Protocol

from src.infra import event_types as ET
from src.infra import git, log_once, models
from src.runtime import control_events, review, run_token, runs, session_dispatch, session_sidebar, task_errors
from src.runtime.hooks import sequence_controllers

if TYPE_CHECKING:
  import asyncio
  import datetime

  from src.runtime import control_sink, session_events, session_store

log = log_once.LazyStructlogLogger()
ARCHIVE_OPERATOR_REFUSAL = "archiving a task requires operator credentials"


class CompletionFacts(Protocol):
  """The folded task facts the completion owner reads; ``task_sessions`` owns the fold."""

  run_outcomes: dict[str, str]
  close_requests: list[dict]
  confirmed_input_ids: set[str]


class CompletionTree(Protocol):
  """The task tree members the completion owner calls; ``TaskTreeManager`` implements them.

  ``index`` is the tree's rebuildable index: the completion owner hands it back to
  the tree and never reads it, so its type is ``Any``.
  """

  control_lock: asyncio.Lock
  events: control_sink.ControlEventSink
  runs: runs.RunStore
  dispatch: session_dispatch.TaskInputDispatcher
  session_events: session_events.SessionEvents
  session_store: session_store.SessionStore

  async def load_meta(self, session_id: str) -> models.SessionMetadata | None:
    ...

  async def load_task_meta(self, session_id: str) -> models.SessionMetadata:
    ...

  def task_state(self, session_id: str) -> str:
    ...

  def fact_history(self, session_id: str) -> list[dict]:
    ...

  def facts_of(self, session_id: str) -> CompletionFacts:
    ...

  async def _get_index(self) -> Any:
    ...

  def _index_meta(self, index: Any, session_id: str) -> models.SessionMetadata:
    ...

  def _children_of(self, index: Any, session_id: str | None) -> list[str]:
    ...

  def _descendants(self, index: Any, session_id: str) -> list[str]:
    ...

  def _invalidate_index(self) -> None:
    ...

  def _host_boot_time(self) -> datetime.datetime:
    ...


# The structured result-ref grammar the evidence checks resolve against the
# task's own facts. Anything else is an opaque evidence pointer (a file path,
# a URL, a ticket id) carried as content — referenced, not proof.
RUN_REF_PREFIX = "run:"
SPEC_REF_PREFIX = "spec:"
REVIEW_REF_PREFIX = "review:"
LANDING_REF_PREFIX = "landed:"


@dataclasses.dataclass(frozen=True)
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


@dataclasses.dataclass(frozen=True)
class CompletionEvidence:
  """The internal evidence bundle a completion claim carries.

  An archive request builds it from the request body; the execution stage's
  automatic worker completion builds it from the run's recorded artifacts.
  ``review_run_ids`` and ``landing`` carry the implement workflow's review
  and target-branch delivery evidence.
  """

  summary: str = ""
  result_refs: list[str] = dataclasses.field(default_factory=list)
  run_ids: list[str] = dataclasses.field(default_factory=list)
  review_run_ids: list[str] = dataclasses.field(default_factory=list)
  landing: LandingEvidence | None = None


# The loop's own outcome a sequence close carries onto the close fact's
# ``sequence_outcome`` field and into the delivered report: "completed" is a
# proven landing, "blocked" a finished loop without proven delivery,
# "failed" a mid-loop failure or restart interruption, "cancelled" a user
# stop. The lifecycle outcome never takes these values; it stays
# "completed" or "cancelled".
SequenceCloseOutcome = Literal["completed", "blocked", "failed", "cancelled"]


class TaskCompletionManager:
  """The close/cancel/reopen owner wired over one TaskTreeManager."""

  def __init__(self, tree: CompletionTree) -> None:
    self._tree = tree

  # ------------------------------------------------------------------
  # Blockers
  # ------------------------------------------------------------------

  def _execution_blockers(
      self,
      index: Any,
      session_id: str,
      *,
      exclude_run_ids: set[str] | None = None,
  ) -> list[str]:
    """The Run blockers plus open-descendant blockers of one task, from current
        facts (lock held by caller). ``index`` is the tree index the caller
        got from ``_get_index``; ``exclude_run_ids`` carves out the Run whose
        own closure request is being re-evaluated.
        """
    tree = self._tree
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
        f"has open descendant task {descendant}" for descendant in tree._descendants(index, session_id)
        if tree.task_state(descendant) == "open")
    return blockers

  def completion_blockers(
      self,
      index: Any,
      session_id: str,
      *,
      exclude_run_ids: set[str] | None,
      exclude_input_ids: set[str] | None,
  ) -> list[str]:
    """The closure blockers of one task, from current facts (lock held by caller).

        Unprocessed input (including later arrivals), active/queued/unresolved
        Runs, and open descendants all block. ``exclude_run_ids`` carves out
        the Run whose own closure request is being re-evaluated;
        ``exclude_input_ids`` carves out exactly that Run's already-claimed
        batch — never later inputs or other Runs' claims.
        """
    blockers = self._execution_blockers(index, session_id, exclude_run_ids=exclude_run_ids)
    pending = [
        e for e in self._tree.dispatch.pending_inputs(session_id)
        if str(e.get("id")) not in (exclude_input_ids or set())
    ]
    if pending:
      blockers.insert(0, session_dispatch.unprocessed_input_blocker(pending))
    return blockers

  def cancellation_blockers(self, index: Any, session_id: str) -> list[str]:
    """The cancellation blockers of one task, from current facts (lock held by caller).

        Explicit cancellation (operator or the owning agent) refuses
        active/unresolved execution and open descendants ONLY (plan 4.1:
        complete blocks on unprocessed input, cancel does not). Unprocessed
        input is preserved history on the cancelled node — refusing cancel
        over it would trap every task whose input nothing consumed yet.
        """
    return self._execution_blockers(index, session_id)

  # ------------------------------------------------------------------
  # Evidence
  # ------------------------------------------------------------------

  def _delivery_run_outcomes(self, index: Any,
                             meta: models.SessionMetadata) -> tuple[dict[str, str | None], dict[str, str]]:
    """The delivery-run universe of one task: its own Runs plus the Runs of
        its direct children (a manager's delivery evidence legitimately cites
        the child work it consumed). Returns (run records by id, outcomes)."""
    tree = self._tree
    records: dict[str, object] = {}
    outcomes: dict[str, str | None] = {}
    for run in tree.runs.list_run_records_sync(meta.id):
      records[run.id] = run
      outcomes[run.id] = tree.facts_of(meta.id).run_outcomes.get(run.id)
    for child_id in tree._children_of(index, meta.id):
      child_facts = tree.facts_of(child_id)
      for run in tree.runs.list_run_records_sync(child_id):
        records[run.id] = run
        outcomes[run.id] = child_facts.run_outcomes.get(run.id)
    return records, outcomes

  def evidence_blockers(
      self,
      index: Any,
      meta: models.SessionMetadata,
      evidence: CompletionEvidence,
      *,
      succeeding_run_ids: set[str] | None = None,
  ) -> list[str]:
    """Validate one completion claim against this task's own facts.

        Pure over the given facts snapshot; callers run it outside the control
        lock and revalidate the relevant facts/spec/refs under the lock
        before appending the close fact. ``succeeding_run_ids`` names Runs
        judged as successful: an own-run request's still-active caller Run,
        whose request is re-evaluated only after it succeeds.
        """
    blockers: list[str] = []
    if not evidence.summary.strip():
      blockers.append("completion requires a non-empty summary")
    if not evidence.result_refs:
      blockers.append("completion requires result evidence (result_refs)")
    tree = self._tree
    records, outcomes = self._delivery_run_outcomes(index, meta)
    for run_id in succeeding_run_ids or ():
      outcomes[run_id] = "success"
    facts = tree.facts_of(meta.id)
    claimed: dict[str, object] = {}
    for run_id in evidence.run_ids:
      run = records.get(run_id)
      if run is None:
        blockers.append(f"run {run_id} is not a Run of task {meta.id} or its children")
        continue
      claimed[run_id] = run
      if outcomes.get(run_id) != "success":
        blockers.append(
            f"run {run_id} has no successful run_finished fact" +
            (" (a bare exit code is not evidence)" if run.exit_code == 0 else ""))
    if evidence.run_ids and not claimed:
      blockers.append("completion evidence names no delivery Run of this task")
    if not evidence.run_ids and not succeeding_run_ids and any(outcome == "success" for outcome in outcomes.values()):
      # Delivery run evidence is required while the delivery universe has
      # a successful Run to cite.
      blockers.append("completion requires delivery run evidence (run_ids)")
    elif not evidence.run_ids and not succeeding_run_ids and any(outcome == "failed" for outcome in outcomes.values()):
      # A failed-only subtree still refuses to be called completed on a
      # bare claim: the failed Run is not completion evidence — cancel
      # the failed work, or retry it to success and cite that Run. Only
      # a delivery
      # universe with neither a successful nor a failed Run (the
      # terminal-driven node's explicit operator completion;
      # cancelled-only or interrupted-only child work) closes on the
      # operator's own attributed evidence (summary + result_refs), and
      # every other closure blocker still applies.
      blockers.append(
          "completion requires delivery run evidence (run_ids); "
          "failed Runs are not completion evidence")
    for ref in evidence.result_refs:
      blockers.extend(self._structured_ref_blockers(meta, ref, records, facts, evidence))
    claimed_work_ids = set(evidence.run_ids)
    for review_run_id in evidence.review_run_ids:
      review_run = records.get(review_run_id)
      if review_run is None or review_run.kind != "review" or review_run.session_id != meta.id:
        blockers.append(f"run {review_run_id} is not a review Run of task {meta.id}")
        continue
      if facts.run_outcomes.get(review_run_id) != "success":
        blockers.append(f"review run {review_run_id} has no successful run_finished fact")
        continue
      # The work/spec/review pin: a review authorizes only the work Run it
      # was chained to, so a historical successful review of a different
      # work attempt never completes this delivery.
      if getattr(review_run, "review_of_run_id", None) is not None:
        if review_run.review_of_run_id not in claimed_work_ids:
          blockers.append(
              f"review run {review_run_id} reviews work run {review_run.review_of_run_id}, "
              "which this completion claim does not name")
      elif claimed_work_ids:
        blockers.append(
            f"review run {review_run_id} is not chained to a work Run "
            "(review_of_run_id is unset; it cannot prove which work it reviewed)")
    if meta.task is not None and meta.task.task_type == models.TaskType.IMPLEMENT:
      successful_reviews = [
          r for r in evidence.review_run_ids if r in records and records[r].kind == "review" and
          records[r].session_id == meta.id and facts.run_outcomes.get(r) == "success" and
          (records[r].review_of_run_id is None or records[r].review_of_run_id in set(evidence.run_ids))
      ]
      review_refs = [r for r in evidence.result_refs if r.startswith(REVIEW_REF_PREFIX)]
      if not successful_reviews and not review_refs:
        blockers.append("implement completion requires a successful review run of this task")
      # The landing requirement rides the repository: a repo-less
      # implement delivery is the reviewer's verdict on the reported
      # paths, and no target-branch landing exists to prove.
      # (one flattened condition: SIM102)
      if (meta.task is not None and meta.task.repo_path and evidence.landing is None and
          not any(r.startswith(LANDING_REF_PREFIX) for r in evidence.result_refs)):
        blockers.append("implement completion requires target-branch landing evidence")
    return blockers

  def _structured_ref_blockers(
      self,
      meta: models.SessionMetadata,
      ref: str,
      records: dict,
      facts: object,
      evidence: CompletionEvidence,
  ) -> list[str]:
    """Resolve one structured result ref against this task's facts.

        run:<id> must be a Run of THIS task; spec:<hash> must be a pinned task
        spec of one of the claimed runs; review:<id> a successful review Run
        of this task; landed:<branch>@<commit> a landing whose branch is a
        branch this task's runs were based on, or the published origin/ ref
        a reviewer pushes it to. Unknown shapes pass through as opaque
        evidence pointers.
        """

    blockers: list[str] = []
    if ref.startswith(RUN_REF_PREFIX):
      run_id = ref[len(RUN_REF_PREFIX):]
      if run_id not in records:
        blockers.append(f"result ref {ref} does not name a Run of task {meta.id}")
    elif ref.startswith(SPEC_REF_PREFIX):
      spec_hash = ref[len(SPEC_REF_PREFIX):]
      claimed_ids = evidence.run_ids or list(records)
      pinned = {records[r].task_spec_hash for r in claimed_ids if r in records}
      pinned.discard(None)
      if spec_hash not in pinned:
        blockers.append(f"result ref {ref} does not match the task spec pinned by this task's runs")
    elif ref.startswith(REVIEW_REF_PREFIX):
      review_id = ref[len(REVIEW_REF_PREFIX):]
      review_run = records.get(review_id)
      if review_run is None or review_run.kind != "review" or review_run.session_id != meta.id:
        blockers.append(f"result ref {ref} does not name a review Run of task {meta.id}")
      elif facts.run_outcomes.get(review_id) != "success":  # type: ignore[union-attr]
        blockers.append(f"result ref {ref} names a review without a successful run_finished fact")
    elif ref.startswith(LANDING_REF_PREFIX):
      body = ref[len(LANDING_REF_PREFIX):]
      branch, _, commit = body.partition("@")
      if not branch or not commit:
        blockers.append(f"result ref {ref} must be landed:<branch>@<commit>")
      else:
        base_branches = {records[r].base_branch for r in records if r in records and records[r].base_branch}
        if meta.task is not None and meta.task.base_branch:
          base_branches.add(meta.task.base_branch)
        # A reviewed Run lands on the branch its reviewer publishes.
        targets = base_branches | {review.review_landing_target(b) for b in base_branches}
        if base_branches and branch not in targets:
          blockers.append(
              f"result ref {ref} lands on {branch}; this task's runs target "
              f"{', '.join(sorted(base_branches))}")
    return blockers

  # ------------------------------------------------------------------
  # Landing verification (the git truth behind a landed: claim)
  # ------------------------------------------------------------------

  def _landing_claims(self, evidence: CompletionEvidence) -> list[tuple[str, str, str | None]]:
    """Every (branch, commit, repo_path override) landing claim one completion carries.

        The structured ``landed:<branch>@<commit>`` result refs and the
        adapter's structured ``landing`` field are the two forms; both pass the
        same git verification, so a forged ref cannot buy what a structured
        bundle cannot.
        """
    claims: list[tuple[str, str, str | None]] = []
    if evidence.landing is not None:
      claims.append((evidence.landing.branch, evidence.landing.commit, evidence.landing.repo_path))
    for ref in evidence.result_refs:
      if not ref.startswith(LANDING_REF_PREFIX):
        continue
      body = ref[len(LANDING_REF_PREFIX):]
      branch, _, commit = body.partition("@")
      if branch and commit:
        claims.append((branch, commit, None))
    return claims

  async def landing_blockers(self, meta: models.SessionMetadata, evidence: CompletionEvidence) -> list[str]:
    """Verify every landing claim against the requested repository and target branch.

        Existence and ancestry run through the existing git helpers OUTSIDE the
        control lock (subprocess work never holds it). A claim that cannot be
        proven — fake commit, real-but-unmerged commit, wrong branch or repo,
        unreadable repository — is an explicit blocker; nothing defaults to
        verified.
        """
    blockers: list[str] = []

    claims = self._landing_claims(evidence)
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
        landed, reason = await git.git_verify_commit_landed(pathlib.Path(repo), branch, commit)
      except OSError as e:
        blockers.append(f"landing {branch}@{commit} verification failed in {repo}: {e}")
        continue
      if not landed:
        blockers.append(f"landing evidence unverified in {repo}: {branch}@{commit}: {reason}")
    return blockers

  async def verified_evidence_blockers(
      self,
      index: Any,
      meta: models.SessionMetadata,
      evidence: CompletionEvidence,
      *,
      succeeding_run_ids: set[str] | None = None,
  ) -> list[str]:
    """The full evidence check: the shape/record layer plus the git landing layer.

        The slow git verification runs outside the control lock; the locked
        revalidation re-runs the shape layer over fresh facts (a landed commit
        cannot un-land, so the outside-lock git verdict stands).
        """
    blockers = self.evidence_blockers(index, meta, evidence, succeeding_run_ids=succeeding_run_ids)
    blockers.extend(await self.landing_blockers(meta, evidence))
    return blockers

  # ------------------------------------------------------------------
  # Archive
  # ------------------------------------------------------------------

  async def archive_task(
      self,
      session_id: str,
      *,
      request_id: str,
      evidence: CompletionEvidence,
      caller: object,
  ) -> tuple[int, dict]:
    """Archive one task: operator calls close its subtree; agent calls wait for their Run.

        An operator closes the open subtree after the unfinished-Run check.
        A run-token agent may archive only its own manager task through its
        active Run; blockers refuse the request before it is saved.
        """

    if not request_id:
      raise task_errors.TaskInvalidError("request_id is required for archive")
    tree = self._tree
    if isinstance(caller, run_token.CallerIdentity) and caller.is_operator:
      replay = self._replay_close_request(session_id, request_id)
      if replay is not None:
        return replay
      archived = await self.archive_subtree(
          session_id,
          request_id=request_id,
          summary=evidence.summary or "archived by the user",
          result_refs=evidence.result_refs,
          run_ids=evidence.run_ids,
          caller=caller)
      return 200, {"archived": archived}

    if isinstance(caller, run_token.CallerIdentity) and not caller.is_operator:
      claims = caller.claims
      assert claims is not None
      if claims.session_id != session_id:
        raise task_errors.TaskForbiddenError("an agent may only archive its own task")
      meta = await tree.load_task_meta(session_id)
      if meta.profile != "manager":
        raise task_errors.TaskForbiddenError("own-task archive requests come from manager tasks only")
      run = await tree.runs.get_run(session_id, claims.run_id)
      if run is None:
        raise task_errors.TaskInvalidError(f"caller run {claims.run_id} not found in task {session_id}")
      events = tree.runs.load_events_sync(session_id)
      if tree.runs.run_has_terminal_fact(run, events) or not tree.runs.run_is_active(run, tree._host_boot_time()):
        raise task_errors.TaskConflictError([f"caller run {claims.run_id} is not active; it cannot request archive"])
      replay = self._replay_close_request(session_id, request_id)
      if replay is not None:
        return replay
      blockers = await self._own_run_close_blockers(session_id, run, evidence)
      if blockers:
        raise task_errors.TaskConflictError(blockers)
      return await self._save_close_request(
          session_id, request_id=request_id, owner_run_id=claims.run_id, evidence=evidence)
    raise task_errors.TaskForbiddenError("task archive requires operator or run-token credentials")

  def archived_close_event(
      self,
      session_id: str,
      request_id: str,
      *,
      summary: str,
      report_to: str | None,
      archived_with: str | None,
      actor: str,
      result_refs: list[str],
      run_ids: list[str],
  ) -> dict:
    """Build an archive close fact with its fixed report recipient and subtree owner."""
    return control_events.build_control_event(
        ET.TASK_CLOSED,
        actor=actor,
        source_session_id=session_id,
        event_id=control_events.stable_close_event_id(session_id, request_id),
        request_id=request_id,
        outcome="archived",
        summary=summary,
        result_refs=list(result_refs),
        run_ids=list(run_ids),
        report_to=report_to,
        **({
            "archived_with": archived_with
        } if archived_with is not None else {}),
    )

  async def archive_subtree(
      self,
      session_id: str,
      *,
      request_id: str | None = None,
      summary: str,
      result_refs: list[str],
      run_ids: list[str],
      caller: object,
  ) -> list[str]:
    """Archive an open task subtree and report the target close to its parent."""
    task_errors.require_operator(caller, ARCHIVE_OPERATOR_REFUSAL)
    if request_id is None:
      request_id = f"archive-{uuid.uuid4()}"
    if not request_id:
      raise task_errors.TaskInvalidError("request_id is required for archive")
    tree = self._tree
    async with tree.control_lock:
      index = await tree._get_index(force=True)
      tree._index_meta(index, session_id)
      if tree.task_state(session_id) != "open":
        return []
      subtree = [session_id, *tree._descendants(index, session_id)]
      blockers = self._archive_run_blockers(subtree)
      if blockers:
        raise task_errors.TaskConflictError(blockers)

    epochs = {sid: await tree.session_events.prime_aggregator(sid) for sid in subtree}
    parent_epoch = None
    if index.metas[session_id].task_parent_id:
      parent_epoch = await tree.session_events.prime_aggregator(str(index.metas[session_id].task_parent_id))

    archived: list[str] = []
    close_announcements: list[tuple[str, dict, int]] = []
    report: dict | None = None
    report_created = False
    parent_id: str | None = None
    async with tree.control_lock:
      index = await tree._get_index(force=True)
      target_meta = tree._index_meta(index, session_id)
      if tree.task_state(session_id) != "open":
        return []
      subtree = [session_id, *tree._descendants(index, session_id)]
      blockers = self._archive_run_blockers(subtree)
      if blockers:
        raise task_errors.TaskConflictError(blockers)
      parent_id = target_meta.task_parent_id
      if parent_id is not None and parent_id not in epochs:
        parent_epoch = await tree.session_events.prime_aggregator(parent_id)
      for sid in subtree:
        if tree.task_state(sid) != "open":
          continue
        is_target = sid == session_id
        if sid not in epochs:
          epochs[sid] = await tree.session_events.prime_aggregator(sid)
        close_event, delivered_report, created = await self._append_closed(
            sid,
            request_id=request_id,
            outcome="archived",
            summary=summary if is_target else "",
            result_refs=result_refs if is_target else [],
            run_ids=run_ids if is_target else [],
            actor=control_events.ACTOR_USER,
            report_to=parent_id if is_target else None,
            archived_with=None if is_target else session_id,
        )
        archived.append(sid)
        close_announcements.append((sid, close_event, epochs[sid]))
        if is_target:
          report = delivered_report
          report_created = created
      for sid in archived:
        binding = sequence_controllers.binding_for(sid)
        if binding is not None:
          await binding.on_archive()

    for sid, event, epoch in close_announcements:
      await tree.session_events.announce_appended_event(sid, event, epoch=epoch)
    if report_created and parent_id is not None and parent_epoch is not None and report is not None:
      await tree.session_events.announce_appended_event(parent_id, report, epoch=parent_epoch)
    if report_created and parent_id is not None and report is not None:
      try:
        await tree.dispatch.wake_parent(parent_id, report=report)
      except Exception as exc:
        log.warning(
            "archive_parent_wake_failed",
            session_id=session_id,
            parent=parent_id,
            report=str(report.get("id")),
            error=str(exc))
    return archived

  def _archive_run_blockers(self, subtree: list[str]) -> list[str]:
    """Return unfinished Run blockers across a subtree before any close fact."""
    tree = self._tree
    host_boot = tree._host_boot_time()
    blockers = [
        f"{sid}: {blocker}" for sid in subtree for run in tree.runs.list_run_records_sync(sid)
        if (blocker := tree.runs.run_blocker(run, tree.runs.load_events_sync(sid), host_boot)) is not None
    ]
    return sorted(blockers)

  async def archive_replaced_source_locked(self, session_id: str, successor_id: str) -> None:
    """Close an elone source with archive facts while its caller holds the tree lock."""
    tree = self._tree
    if tree.task_state(session_id) != "open":
      return
    await self._append_closed(
        session_id,
        request_id=f"elone-{uuid.uuid4()}",
        outcome="archived",
        summary=f"archived by elone successor {successor_id}",
        result_refs=[],
        run_ids=[],
        actor=control_events.ACTOR_USER,
        report_to=None,
        archived_with=None,
    )

  async def archive_unsuccessful_worker(self, session_id: str, *, run_id: str, outcome: str) -> bool:
    """Archive a failed worker after its report lands, when no work remains."""
    tree = self._tree
    epoch = await tree.session_events.prime_aggregator(session_id)
    request_id = f"unsuccessful:{run_id}"
    async with tree.control_lock:
      index = await tree._get_index()
      meta = tree._index_meta(index, session_id)
      if meta.profile != "worker" or tree.task_state(session_id) != "open":
        return False
      if tree.runs.last_ended_run_id(session_id) != run_id:
        return False
      if self._execution_blockers(index, session_id):
        return False
      if self.pause_reason(session_id) is not None:
        return False
      if any(event.get("type") == ET.TASK_CLOSED and event.get("request_id") == request_id
             for event in tree.fact_history(session_id)):
        return False
      close_event, _report, _report_created = await self._append_closed(
          session_id,
          request_id=request_id,
          outcome="archived",
          summary=f"run {run_id} ended {outcome}; archived by the server",
          result_refs=[],
          run_ids=[run_id],
          actor=control_events.ACTOR_SYSTEM,
          report_to=None,
          archived_with=None)
      binding = sequence_controllers.binding_for(session_id)
      if binding is not None:
        await binding.on_archive()
    await tree.session_events.announce_appended_event(session_id, close_event, epoch=epoch)
    return True

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

  async def _own_run_close_blockers(self, session_id: str, run: models.RunRecord,
                                    evidence: CompletionEvidence) -> list[str]:
    """The blockers an own-run closure request faces now (lock held inside).

        The close path's checks, minus what exists only because the caller
        Run is still active — the carve-outs ``recheck_close_requests`` makes
        once that Run succeeds: the Run itself (no active-Run blocker, and
        judged successful as evidence) and the input batch it already claimed.
        """
    tree = self._tree
    async with tree.control_lock:
      index = await tree._get_index()
      tree._index_meta(index, session_id)
      blockers = self.completion_blockers(
          index, session_id, exclude_run_ids={run.id}, exclude_input_ids=set(run.input_event_ids))
      meta = await tree.load_meta(session_id)
      assert meta is not None
      blockers.extend(self.evidence_blockers(index, meta, evidence, succeeding_run_ids={run.id}))
    blockers.extend(await self.landing_blockers(meta, evidence))
    return sorted(set(blockers))

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
      event = control_events.build_control_event(
          ET.TASK_CLOSE_REQUESTED,
          actor=control_events.ACTOR_SYSTEM,
          source_session_id=session_id,
          event_id=control_events.stable_close_request_event_id(session_id, request_id),
          request_id=request_id,
          owner_run_id=owner_run_id,
          summary=evidence.summary,
          result_refs=list(evidence.result_refs),
          run_ids=list(evidence.run_ids),
      )
      await tree.events.append(session_id, event)
    log.info("task_close_requested", session_id=session_id, request_id=request_id, owner_run_id=owner_run_id)
    return 202, {"session_id": session_id, "request_id": request_id, "status": "pending_run_finish"}

  async def _close_now(
      self,
      session_id: str,
      *,
      request_id: str,
      evidence: CompletionEvidence,
      actor: str,
      outcome: str,
      exclude_run_ids: set[str] | None = None,
      exclude_input_ids: set[str] | None = None,
  ) -> tuple[int, dict]:
    """Evaluate, validate, and append one close fact.

        Facts are snapshotted and checked under the lock, evidence validation
        runs outside it, and the relevant facts/spec/refs are revalidated
        under the lock before the close fact lands.
        """
    tree = self._tree
    async with tree.control_lock:
      index = await tree._get_index()
      tree._index_meta(index, session_id)
      blockers = self.completion_blockers(
          index, session_id, exclude_run_ids=exclude_run_ids, exclude_input_ids=exclude_input_ids)
      meta = await tree.load_meta(session_id)
      assert meta is not None
    if blockers:
      raise task_errors.TaskConflictError(sorted(set(blockers)))
    # The potentially slow evidence validation — including the git landing
    # verification — runs outside the control lock and is authoritative:
    # an unproven claim (forged hash, unlanded commit, wrong branch or
    # repo) aborts the close here. The locked pass below revalidates the
    # fast facts (structure plus the shape layer) — a verified landing
    # cannot un-land between the two passes, and a moving condition
    # (run finished, input processed) is caught by the fresh structural
    # and shape revalidation.
    succeeding_run_ids = exclude_run_ids if outcome == "archived" else None
    evidence_blockers = await self.verified_evidence_blockers(
        index, meta, evidence, succeeding_run_ids=succeeding_run_ids)
    if evidence_blockers:
      raise task_errors.TaskConflictError(sorted(set(evidence_blockers)))
    # Live-announce epochs are taken before any append, so the announce
    # feed can never double-render an event the aggregator caught up on.
    child_epoch = await tree.session_events.prime_aggregator(session_id)
    parent_epoch = (await tree.session_events.prime_aggregator(meta.task_parent_id) if meta.task_parent_id else None)
    async with tree.control_lock:
      fresh_index = await tree._get_index()
      tree._index_meta(fresh_index, session_id)
      if tree.task_state(session_id) != "open":
        raise task_errors.TaskConflictError([f"task {session_id} is no longer open"])
      fresh_blockers = self.completion_blockers(
          fresh_index, session_id, exclude_run_ids=exclude_run_ids, exclude_input_ids=exclude_input_ids)
      fresh_meta = await tree.load_meta(session_id)
      assert fresh_meta is not None
      fresh_evidence_blockers = self.evidence_blockers(
          fresh_index, fresh_meta, evidence, succeeding_run_ids=succeeding_run_ids)
      all_blockers = sorted(set(fresh_blockers + fresh_evidence_blockers))
      if all_blockers:
        # The locked revalidation is authoritative: conditions changed
        # during the outside-the-lock validation leave the task open
        # with the current, visible blockers.
        raise task_errors.TaskConflictError(all_blockers)
      close_event, report, report_created = await self._append_closed(
          session_id,
          request_id=request_id,
          outcome=outcome,
          summary=evidence.summary,
          result_refs=evidence.result_refs,
          run_ids=evidence.run_ids,
          actor=actor,
          report_to=fresh_meta.task_parent_id,
          archived_with=None)
    await self._announce_close_and_wake_parent(
        session_id,
        fresh_meta,
        close_event,
        report,
        report_created,
        child_epoch=child_epoch,
        parent_epoch=parent_epoch,
        wake_log_event="close_parent_wake_failed")
    return 200, {"session_id": session_id, "closed_event_id": close_event["id"]}

  async def _append_closed(
      self,
      session_id: str,
      *,
      request_id: str,
      outcome: str,
      summary: str,
      result_refs: list[str],
      run_ids: list[str],
      actor: str,
      report_to: str | None,
      archived_with: str | None,
      sequence_outcome: str | None = None,
  ) -> tuple[dict, dict, bool]:
    """Append one task_closed fact, then deliver its parent report.

        Lock held by the caller. The report's outcome, summary, and
        result_refs are the close fact's own fields — the outcome reads
        ``sequence_outcome`` when the close carries one, the lifecycle
        outcome otherwise: recovery re-derives a replayed report from the
        close fact, so a live delivery carrying different values would make
        the parent's view depend on which pass landed first. report_to fixes
        this closure's delivery ownership at close time; retries, recovery,
        and reparenting can never retarget it. The report lands before the
        lock releases (a control fact); the live announcements follow outside
        it (the caller's).
        """
    tree = self._tree
    payload: dict[str, Any] = {}
    if sequence_outcome is not None:
      # The loop's own outcome: only a sequence close carries this field.
      payload["sequence_outcome"] = sequence_outcome
    if outcome == "archived":
      close_event = self.archived_close_event(
          session_id,
          request_id,
          summary=summary,
          report_to=report_to,
          archived_with=archived_with,
          actor=actor,
          result_refs=result_refs,
          run_ids=run_ids)
    elif outcome in ("completed", "cancelled"):
      close_event = control_events.build_control_event(
          ET.TASK_CLOSED,
          actor=actor,
          source_session_id=session_id,
          event_id=control_events.stable_close_event_id(session_id, request_id),
          request_id=request_id,
          outcome=outcome,
          summary=summary,
          result_refs=list(result_refs),
          run_ids=list(run_ids),
          report_to=report_to,
          **payload,
      )
    else:
      raise ValueError(f"unknown task close outcome: {outcome}")
    await tree.events.append(session_id, close_event)
    report, report_created = await tree.dispatch.deliver_child_report_locked(
        session_id,
        source_event=close_event,
        outcome=sequence_outcome if sequence_outcome is not None else outcome,
        summary=summary,
        result_refs=list(result_refs),
        recipient=report_to,
        actor=control_events.ACTOR_SYSTEM,
    )
    tree._invalidate_index()
    return close_event, report, report_created

  async def _announce_close_and_wake_parent(
      self,
      session_id: str,
      meta: models.SessionMetadata,
      close_event: dict,
      report: dict,
      report_created: bool,
      *,
      child_epoch: int,
      parent_epoch: int | None,
      wake_log_event: str,
  ) -> None:
    """Announce a landed close and hand its report to the parent as a wake input.

    The close fact and its report are already durable here, so a failed wake is
    logged and the close stands: any later dispatch or recovery pass launches
    the parent's queued reservation with the report. Failing the close instead
    would let an automatic-completion caller classify the landed boundary as
    blocked and deliver a contradictory second report over it.
    """
    tree = self._tree
    await tree.session_events.announce_appended_event(session_id, close_event, epoch=child_epoch)
    if report_created and parent_epoch is not None:
      await tree.session_events.announce_appended_event(str(meta.task_parent_id), report, epoch=parent_epoch)
    if report_created and meta.task_parent_id:
      try:
        await tree.dispatch.wake_parent(str(meta.task_parent_id), report=report)
      except Exception as exc:
        log.warning(
            wake_log_event,
            session_id=session_id,
            parent=str(meta.task_parent_id),
            report=str(report.get("id")),
            error=str(exc))

  async def close_sequence_child(
      self,
      session_id: str,
      *,
      request_id: str,
      outcome: SequenceCloseOutcome,
      evidence: CompletionEvidence,
  ) -> tuple[int, dict]:
    """The sequence owner's close of its ended loop's worker child: one close fact, one parent report.

        The caller is the server itself (a sequence controller), so the close
        fact records the system actor. The lifecycle outcome is "completed"
        only when the loop's own outcome is "completed" AND the evidence
        names at least one Run — that claim passes the same checks the
        operator close runs (``verified_evidence_blockers`` outside the
        control lock, the shape layer revalidated under it). Every other
        outcome closes as "cancelled" with the evidence recorded as content
        only: a cancelled lifecycle never claims successful delivery.
        Blockers are the cancellation rule — an active Run, a queued Run
        without a stop request, an unresolved process, or an open descendant;
        unprocessed input never blocks. The close fact carries the loop's own
        outcome as ``sequence_outcome`` and the delivered report takes it as
        its outcome, so the parent reads exactly what the loop reported. A
        replayed (session, request_id) returns the original close — including
        one arriving after the node was reopened.
        """
    if not request_id:
      raise task_errors.TaskInvalidError("request_id is required for sequence close")
    tree = self._tree
    replay = self._replay_close_request(session_id, request_id)
    if replay is not None:
      return replay
    completing = outcome == "completed" and bool(evidence.run_ids)
    pre_meta = await tree.load_task_meta(session_id)
    async with tree.control_lock:
      index = await tree._get_index()
      tree._index_meta(index, session_id)
      if tree.task_state(session_id) != "open":
        raise task_errors.TaskConflictError([f"task {session_id} is no longer open"])
      blockers = self.cancellation_blockers(index, session_id)
    if blockers:
      raise task_errors.TaskConflictError(sorted(set(blockers)))
    if completing:
      # The potentially slow evidence validation — the git landing
      # verification included — runs outside the control lock and is
      # authoritative, exactly as the operator close runs it; the locked
      # pass below revalidates the fast shape layer over fresh facts.
      evidence_blockers = await self.verified_evidence_blockers(index, pre_meta, evidence)
      if evidence_blockers:
        raise task_errors.TaskConflictError(sorted(set(evidence_blockers)))
    child_epoch = await tree.session_events.prime_aggregator(session_id)
    parent_epoch = (
        await tree.session_events.prime_aggregator(pre_meta.task_parent_id) if pre_meta.task_parent_id else None)
    async with tree.control_lock:
      index = await tree._get_index()
      tree._index_meta(index, session_id)
      # The replay re-check leads: a raced close under this request id — the
      # other call point of the same loop-close step — returns the first
      # result, whatever state the node has since reached.
      replay = self._replay_close_request(session_id, request_id)
      if replay is not None:
        return replay
      if tree.task_state(session_id) != "open":
        raise task_errors.TaskConflictError([f"task {session_id} is no longer open"])
      fresh_meta = await tree.load_meta(session_id)
      assert fresh_meta is not None
      fresh_blockers = self.cancellation_blockers(index, session_id)
      if completing:
        fresh_blockers += self.evidence_blockers(index, fresh_meta, evidence)
      if fresh_blockers:
        # The locked revalidation is authoritative: conditions that changed
        # during the outside-the-lock validation leave the task open with
        # the current, visible blockers.
        raise task_errors.TaskConflictError(sorted(set(fresh_blockers)))
      close_event, report, report_created = await self._append_closed(
          session_id,
          request_id=request_id,
          outcome="completed" if completing else "cancelled",
          summary=evidence.summary,
          result_refs=evidence.result_refs,
          run_ids=evidence.run_ids,
          actor=control_events.ACTOR_SYSTEM,
          report_to=fresh_meta.task_parent_id,
          archived_with=None,
          sequence_outcome=outcome)
    await self._announce_close_and_wake_parent(
        session_id,
        fresh_meta,
        close_event,
        report,
        report_created,
        child_epoch=child_epoch,
        parent_epoch=parent_epoch,
        wake_log_event="sequence_close_parent_wake_failed")
    return 200, {"session_id": session_id, "closed_event_id": close_event["id"]}

  # ------------------------------------------------------------------
  # Own-run close re-evaluation and automatic worker completion
  # ------------------------------------------------------------------

  async def recheck_close_requests(self, session_id: str, run_id: str) -> list[str]:
    """Re-evaluate one Run's pending closure requests after it finished successfully.

        Recovery replays the same request once (the stable request id dedups);
        changed conditions leave the task open and wake the requesting session
        with the blockers (one notice per request id). Returns the blockers
        that kept each request open (empty when a close landed or nothing was
        pending).
        """
    tree = self._tree
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
            session_id,
            request_id=request_id,
            evidence=evidence,
            actor=control_events.ACTOR_SYSTEM,
            outcome="archived",
            exclude_run_ids={run_id},
            exclude_input_ids=claimed)
      except task_errors.TaskConflictError as e:
        # Changed conditions leave the task open with visible blockers.
        blockers = list(getattr(e, "blockers", None) or [])
        remaining.extend(blockers)
        log.info("task_close_request_still_blocked", session_id=session_id, request_id=request_id, blockers=blockers)
        if tree.task_state(session_id) == "open":
          await self._notify_close_request_blocked(session_id, request_id, blockers)
    return remaining

  async def _notify_close_request_blocked(self, session_id: str, request_id: str, blockers: list[str]) -> None:
    """Wake the requesting session with the blockers its saved request hit.

        A server-minted scheduled input (actor system, never a user event, so
        the runtime gate never counts it as a user message) under a stable id
        per request: recovery replays of the same request admit nothing new.
        The notice targets open nodes only.
        """
    tree = self._tree
    lines = "\n".join(f"- {b}" for b in blockers)
    try:
      await tree.dispatch.admit_input(
          session_id,
          event_type=ET.SCHEDULED_TRIGGER,
          content=(
              f"Closure request {request_id} did not close this task. Blockers:\n{lines}\n"
              "Resolve them, then send a new closure request."),
          actor=control_events.ACTOR_SYSTEM,
          input_id=control_events.stable_close_blocked_notice_id(session_id, request_id),
      )
    except task_errors.TaskArchivedError as e:
      # The notice targets open nodes only: the node archived between the
      # caller's open check and this admission, and an archived node keeps no
      # pending inputs — the notice has nowhere to land.
      log.info("close_blocked_notice_skipped_archived", session_id=session_id, error=str(e))
      return
    await tree.dispatch.dispatch_pending(session_id)

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
    tree = self._tree
    meta = await tree.load_task_meta(session_id)
    if meta.profile != "worker":
      raise task_errors.TaskForbiddenError("automatic completion applies to worker tasks")
    facts = tree.facts_of(session_id)
    if facts.run_outcomes.get(run_id) != "success":
      raise task_errors.TaskConflictError(
          [
              (
                  f"run {run_id} has no successful run_finished fact; "
                  "failed or interrupted evidence keeps the task open")
          ])
    run = await tree.runs.get_run(session_id, run_id)
    if run is None:
      raise task_errors.TaskNotFoundError(runs.run_not_found_in_task_text(run_id, session_id))
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
        session_id,
        request_id=effective_request_id,
        evidence=evidence,
        actor=control_events.ACTOR_SYSTEM,
        outcome="completed")

  def pause_reason(self, session_id: str) -> str | None:
    """The node's pause reason, or None when nothing waits.

        A run that ends while its node has a pending trigger (its triggers
        directory) or a pending input (input facts no Run claimed) is a pause,
        not a delivery: the node stays open on its branch and worktree, and the
        wake's own run continues. The one definition every delivery entry asks
        first; it reads disk facts only and adds no state of its own.
        """
    triggers_dir = self._tree.session_store.session_dir(session_id) / "triggers"
    pending_triggers, _next_fire = session_sidebar.pending_trigger_state_sync(triggers_dir, None, None)
    if pending_triggers:
      return f"has {pending_triggers} pending trigger(s)"
    pending = self._tree.dispatch.pending_inputs(session_id)
    if pending:
      return session_dispatch.unprocessed_input_blocker(pending)
    return None

  async def after_run_finished(self, session_id: str, run_id: str) -> None:
    """The one post-success follow-up owner, driven by the durable outcome.

        A successful Run re-evaluates its manager's pending close requests. A
        successful worker WORK Run evaluates automatic completion unless the
        task's delivery rule waits for more evidence — an implement task's
        delivery waits for its review — plus the target-branch landing when
        the task carries a repository — which the execution adapter drives
        through :meth:`evaluate_automatic_completion` with the full verified
        bundle. A cold-read task's delivery is its report body, which the
        execution adapter writes before driving the same method, so the
        generic path below skips it rather than stamp the default body. A
        blocked automatic close keeps its blockers visible and the adapters
        re-evaluate.
        """
    tree = self._tree
    meta = await tree.load_task_meta(session_id)
    await self.recheck_close_requests(session_id, run_id)
    facts = tree.facts_of(session_id)
    if facts.run_outcomes.get(run_id) != "success":
      return
    run = await tree.runs.get_run(session_id, run_id)
    if run is None or meta.profile != "worker" or run.kind != "work":
      return
    if meta.task is not None and meta.task.task_type == models.TaskType.IMPLEMENT:
      log.info("worker_delivery_awaits_review", session_id=session_id, run_id=run_id)
      return
    if meta.task is not None and meta.task.task_type == models.TaskType.COLD_READ:
      # The execution layer writes the report body and completes through
      # evaluate_automatic_completion itself; the default body must not land first.
      log.info("cold_read_delivery_awaits_report", session_id=session_id, run_id=run_id)
      return
    pause = self.pause_reason(session_id)
    if pause is not None:
      # A paused run is not a delivery: the wake's continuation run
      # re-evaluates completion when it ends with nothing pending.
      log.info("worker_delivery_paused", session_id=session_id, run_id=run_id, reason=pause)
      return
    try:
      await self.evaluate_automatic_completion(session_id, run_id=run_id)
    except task_errors.TaskConflictError as e:
      log.info(
          "automatic_worker_completion_blocked",
          session_id=session_id,
          run_id=run_id,
          blockers=getattr(e, "blockers", None))

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
    task_errors.require_operator(caller, "acknowledging task input requires operator credentials")
    if not request_id:
      raise task_errors.TaskInvalidError("request_id is required for input acknowledgement")
    if not input_ids:
      raise task_errors.TaskInvalidError("input_ids is required and must name at least one input")
    if len(set(input_ids)) != len(input_ids):
      raise task_errors.TaskInvalidError("input_ids must not repeat an id")
    tree = self._tree
    replay = self._replay_input_ack(session_id, request_id)
    if replay is not None:
      return replay
    epoch = await tree.session_events.prime_aggregator(session_id)
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
        raise task_errors.TaskConflictError([session_dispatch.inputs_not_pending_conflict(session_id, unknown)])
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
      event = control_events.build_control_event(
          ET.TASK_INPUT_ACKNOWLEDGED,
          actor=control_events.ACTOR_USER,
          source_session_id=session_id,
          event_id=control_events.stable_input_ack_event_id(session_id, request_id),
          request_id=request_id,
          input_ids=acked_now,
          note=note,
      )
      await tree.events.append(session_id, event)
      tree._invalidate_index()
    await tree.session_events.announce_appended_event(session_id, event, epoch=epoch)
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
    event_id = control_events.stable_input_ack_event_id(session_id, request_id)
    for event in tree.fact_history(session_id):
      if (event.get("type") == ET.TASK_INPUT_ACKNOWLEDGED and event.get("id") == event_id):
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

  def _require_open_for_cancel(self, session_id: str) -> None:
    """Refuse a cancel of a task that is not open, naming the state it is in.

    A cancel applies to an open task only, on both the manager and the worker path. The state is
    read once so the refusal sentence and the test that produced it name the same value.
    """
    state = self._tree.task_state(session_id)
    if state != "open":
      raise task_errors.TaskConflictError([f"task {session_id} is {state}; only an open task can be cancelled"])

  async def cancel_task(
      self,
      session_id: str,
      *,
      request_id: str,
      reason: str,
      caller: object,
  ) -> dict:
    """Explicit cancellation with reason, preserving all evidence.

        An operator cancels any open task; an agent cancels only a direct
        child of its own task (the session its verified run token binds). A
        worker cancellation closes the task and requests stops for its
        non-terminal Runs; a manager cancellation keeps the existing blockers.
        Duplicate request ids replay the original close only to a caller
        authorized for that cancel.
        """

    if not isinstance(caller, run_token.CallerIdentity):
      raise task_errors.TaskForbiddenError("task cancellation requires operator credentials")
    if caller.is_operator:
      actor = control_events.ACTOR_USER
      target_meta = None
    else:
      # Agent scope, decided before the replay lookup: a replayed
      # request id returns its original outcome only to a caller
      # authorized for that cancel — the same rule create_task applies
      # to replays. The token's bound session IS the agent's own task.
      claims = caller.claims
      assert claims is not None  # a verified agent caller always carries its claims
      target_meta = await self._tree.load_task_meta(session_id)
      if target_meta.task_parent_id != claims.session_id:
        raise task_errors.TaskForbiddenError("an agent may only cancel a direct child of its own task")
      actor = control_events.ACTOR_AGENT
    if not request_id:
      raise task_errors.TaskInvalidError("request_id is required for cancellation")
    if not reason.strip():
      raise task_errors.TaskInvalidError("cancellation requires a reason")
    tree = self._tree
    if target_meta is None:
      target_meta = await tree.load_task_meta(session_id)
    if target_meta.profile == "worker":
      return await self._cancel_worker_task(
          session_id, request_id=request_id, reason=reason, actor=actor, target_meta=target_meta)
    replay = self._replay_close_request(session_id, request_id)
    if replay is not None:
      return replay[1]
    pre_meta = target_meta
    child_epoch = await tree.session_events.prime_aggregator(session_id)
    parent_epoch = (
        await tree.session_events.prime_aggregator(pre_meta.task_parent_id) if pre_meta.task_parent_id else None)
    async with tree.control_lock:
      index = await tree._get_index()
      meta = tree._index_meta(index, session_id)
      self._require_open_for_cancel(session_id)
      replay = self._replay_close_request(session_id, request_id)
      if replay is not None:
        return replay[1]
      blockers = self.cancellation_blockers(index, session_id)
      if blockers:
        raise task_errors.TaskConflictError(sorted(set(blockers)))
      close_event, report, report_created = await self._append_closed(
          session_id,
          request_id=request_id,
          outcome="cancelled",
          summary=reason,
          result_refs=[],
          run_ids=[],
          actor=actor,
          report_to=meta.task_parent_id,
          archived_with=None)
    await self._announce_close_and_wake_parent(
        session_id,
        meta,
        close_event,
        report,
        report_created,
        child_epoch=child_epoch,
        parent_epoch=parent_epoch,
        wake_log_event="cancel_parent_wake_failed")
    return {"session_id": session_id, "closed_event_id": close_event["id"]}

  async def _cancel_worker_task(
      self,
      session_id: str,
      *,
      request_id: str,
      reason: str,
      actor: str,
      target_meta: models.SessionMetadata,
  ) -> dict:
    """Close one worker and request its non-sequence Run stops under one lock."""
    tree = self._tree
    child_epoch = await tree.session_events.prime_aggregator(session_id)
    parent_epoch = (
        await tree.session_events.prime_aggregator(target_meta.task_parent_id) if target_meta.task_parent_id else None)
    close_event: dict | None = None
    report: dict = {}
    report_created = False
    async with tree.control_lock:
      index = await tree._get_index()
      meta = tree._index_meta(index, session_id)
      replay = self._replay_close_request(session_id, request_id)
      if replay is None:
        self._require_open_for_cancel(session_id)
        open_descendants = [
            descendant for descendant in tree._descendants(index, session_id) if tree.task_state(descendant) == "open"
        ]
        if open_descendants:
          raise task_errors.TaskConflictError(
              [f"has open descendant task {descendant}" for descendant in open_descendants])
        events = tree.runs.load_events_sync(session_id)
        run_records = tree.runs.list_run_records_sync(session_id)
        sequence_blockers = [
            f"run {run.id} is owned by a sequence controller" for run in run_records
            if not tree.runs.run_has_terminal_fact(run, events) and run.sequence_ref is not None
        ]
        if sequence_blockers:
          raise task_errors.TaskConflictError(sequence_blockers)
        stoppable_runs = [run for run in run_records if not tree.runs.run_has_terminal_fact(run, events)]
        close_event, report, report_created = await self._append_closed(
            session_id,
            request_id=request_id,
            outcome="cancelled",
            summary=reason,
            result_refs=[],
            run_ids=[],
            actor=actor,
            report_to=meta.task_parent_id,
            archived_with=None)
        for run in stoppable_runs:
          appended = await tree.runs.append_stop_request_locked(session_id, run.id, f"cancel:{request_id}")
          assert appended, f"run {run.id} became terminal while the worker cancel lock was held"
      elif replay[0] == 202:
        return {**replay[1], "stopped_runs": []}

      stop_request_id = f"cancel:{request_id}"
      stop_runs = self._cancel_stop_runs(session_id, stop_request_id)

    identity_error: runs.RunIdentityConflictError | None = None
    for run in stop_runs:
      if run.pid is None:
        continue
      try:
        await tree.runs.reconcile_stop_request(session_id, run.id)
      except runs.RunIdentityConflictError as exc:
        if identity_error is None:
          identity_error = exc
        else:
          log.warning("worker_cancel_stop_identity_conflict", session_id=session_id, run_id=run.id, error=str(exc))

    if close_event is not None:
      await self._announce_close_and_wake_parent(
          session_id,
          target_meta,
          close_event,
          report,
          report_created,
          child_epoch=child_epoch,
          parent_epoch=parent_epoch,
          wake_log_event="cancel_parent_wake_failed")
    if identity_error is not None:
      raise identity_error
    return {
        "session_id": session_id,
        "closed_event_id": replay[1]["closed_event_id"] if close_event is None else close_event["id"],
        "stopped_runs": self._cancel_stopped_run_results(session_id, stop_request_id),
    }

  def _cancel_stop_runs(self, session_id: str, request_id: str) -> list[models.RunRecord]:
    """Runs named by one cancellation's durable stop facts, in fact order."""
    tree = self._tree
    events = tree.runs.load_events_sync(session_id)
    stopped_ids = list(
        dict.fromkeys(
            str(event["run_id"])
            for event in events
            if event.get("type") == ET.RUN_STOP_REQUESTED and event.get("request_id") == request_id))
    records = {run.id: run for run in tree.runs.list_run_records_sync(session_id)}
    return [records[run_id] for run_id in stopped_ids]

  def _cancel_stopped_run_results(self, session_id: str, request_id: str) -> list[dict]:
    """The cancellation's durable Run set with outcomes from current facts."""
    tree = self._tree
    events = tree.runs.load_events_sync(session_id)
    return [
        {
            "run_id": run.id,
            "outcome": "never_launched" if run.pid is None else tree.runs.terminal_outcome(events, run.id),
        } for run in self._cancel_stop_runs(session_id, request_id)
    ]

  async def restore_task(
      self,
      session_id: str,
      *,
      request_id: str,
      reason: str,
      caller: object,
  ) -> dict:
    """Operator restore of one archived task plus its archived ancestor chain.

    The chain writes from the topmost archived ancestor down to the target —
    the invariant (every ancestor of an open task is open) leaves no other
    admissible order. Siblings and descendants stay archived, no round starts,
    and each fact's reason names the restore's source. A duplicate request id
    restores nothing twice (per-node replay check); a target that is already
    open restores nothing and returns an empty list.
    """
    task_errors.require_operator(caller, "task restore requires operator credentials")
    if not request_id:
      raise task_errors.TaskInvalidError("request_id is required for restore")
    restored = await self.restore_chain(session_id, request_id=request_id, reason=reason)
    return {"session_id": session_id, "restored": restored}

  async def restore_chain(self, session_id: str, *, request_id: str, reason: str) -> list[str]:
    """The restore entry for callers that bring their own authorization (the
    scheduler's auto-bind and enable restores): takes the control lock,
    writes the chain's facts, and announces them after the lock releases."""
    async with self._tree.control_lock:
      restored, announcements = await self.restore_chain_locked(session_id, request_id=request_id, reason=reason)
    for node_id, event, epoch in announcements:
      await self._tree.session_events.announce_appended_event(node_id, event, epoch=epoch)
    return restored

  async def restore_chain_locked(self, session_id: str, *, request_id: str,
                                 reason: str) -> tuple[list[str], list[tuple[str, dict, int]]]:
    """The restore's core, for callers already holding the control lock.

    Writes one ``task_reopened`` fact per archived node from the topmost
    archived ancestor down to *session_id*. Returns the restored ids in that
    order plus the (node, event, epoch) triples the caller announces after
    releasing the lock. A replayed request id skips its already-restored
    nodes, so a crash between facts replays into the same single set.
    """
    tree = self._tree
    index = await tree._get_index()
    if index.metas.get(session_id) is None:
      raise task_errors.task_not_found(session_id)
    # The archived chain above the target, nearest first; the target joins
    # when it is itself archived. The walk stops at the first open node.
    chain: list[str] = []
    current = session_id
    seen: set[str] = set()
    while True:
      meta = tree._index_meta(index, current)
      if tree.task_state(current) == "open":
        break
      chain.append(current)
      if meta.task_parent_id is None or meta.task_parent_id in seen:
        break
      seen.add(current)
      current = meta.task_parent_id
      if len(chain) > task_errors.RESTORE_CHAIN_HOP_LIMIT:
        raise task_errors.TaskConflictError(
            [f"ancestor chain of {session_id} exceeds {task_errors.RESTORE_CHAIN_HOP_LIMIT} hops"])
    if not chain:
      return [], []
    restored: list[str] = []
    announcements: list[tuple[str, dict, int]] = []
    for node_id in reversed(chain):  # topmost first
      history = tree.fact_history(node_id)
      lifecycle = [e for e in history if e.get("type") in (ET.TASK_CLOSED, ET.TASK_REOPENED)]
      if lifecycle and lifecycle[-1].get("type") == ET.TASK_REOPENED and \
          lifecycle[-1].get("request_id") == request_id:
        restored.append(node_id)  # this request already restored the node
        continue
      closes = [e for e in history if e.get("type") == ET.TASK_CLOSED]
      last_close = closes[-1] if closes else None
      # The fact's identity scopes to the close it ends, so a replayed request
      # that meets a NEWER close writes a fresh fact instead of colliding with
      # the one its earlier pass already landed.
      event_scope = request_id if last_close is None else f"{request_id}:{last_close.get('id')}"
      reopen_event = control_events.build_control_event(
          ET.TASK_REOPENED,
          actor=control_events.ACTOR_USER,
          source_session_id=node_id,
          event_id=control_events.stable_reopen_event_id(node_id, event_scope),
          request_id=request_id,
          closed_event_id=str(last_close.get("id")) if last_close is not None else None,
          reason=reason,
      )
      epoch = await tree.session_events.prime_aggregator(node_id)
      await tree.events.append(node_id, reopen_event)
      restored.append(node_id)
      announcements.append((node_id, reopen_event, epoch))
    tree._invalidate_index()
    return restored, announcements
