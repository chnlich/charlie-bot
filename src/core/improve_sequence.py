"""The v2 improve loop: one worker child task, iteration Runs, one final report.

The improve controller keeps its existing canonical state — the per-loop
directory under ``sessions/<id>/loops/<loop_id>/`` with ``goal.md`` (re-read at
every iteration), the optional ``plan.md``, and ``state.json``
(:class:`~src.core.improve_command.ImproveState`) — and its existing stop
convention (goal reached, iterations exhausted, or explicitly stopped). What
changes is where executions live: instead of legacy worker threads, the loop
owns ONE worker child task under the calling manager, and every iteration is
one Run on that child (``kind="iteration"``,
``sequence_ref=(improve, owner_ref=<loop dir>, position=<n>)``).

Boundaries this module pins:

- Progression stays with this controller: each iteration is launched through
  the shared execution adapter and awaited to its durable terminal fact, then
  judged by the same mechanical validity/quota rules as before. No per-iteration
  master wake exists on the v2 path — each result is recorded (run fact + loop
  report + chat progress event) and only the final sequence result is delivered
  through the common report owner, once, to the fixed parent.
- One successful iteration never closes the child: the controller decides the
  overall outcome after the loop ends. Exhausting the iterations without
  proving the goal is NOT successful delivery — the final report says so
  (``blocked`` when a decision is owed, ``failed`` on a quota/loop failure,
  ``cancelled`` when the user stopped the loop) and the child stays open with
  its evidence. ``completed`` requires the deliverable to be real: a
  requested merge-back that actually landed.
- Re-entry is stable: the child's id derives from (parent, ``improve:<loop
  id>``) and each iteration's Run id from (child, ``improve:<loop id>:iter:<n>``),
  so a replayed admission or a repeated finalization never duplicates a child
  or an iteration.
- After a restart the launched iteration is reconciled by the startup owner,
  but the whole loop is never automatically resumed (the existing improve
  boundary). The interrupted controller is reported honestly: the loop state
  is marked ``interrupted``, the stale active lock is cleared, and the chat
  stream carries one visible notice.
"""

from __future__ import annotations

import asyncio
import os
from pathlib import Path
from typing import TYPE_CHECKING

from src.core import event_types as ET
from src.core import git, improve_command
from src.core.config import CharlieBotConfig
from src.core.control_events import stable_run_id
from src.core.log_once import LazyStructlogLogger
from src.core.models import RunRecord, SequenceRef, SessionMetadata, TaskSpec

if TYPE_CHECKING:
    from src.core.task_sessions import TaskTreeManager

log = LazyStructlogLogger()


def loop_owner_ref(session_id: str, loop_id: int, cfg: CharlieBotConfig) -> str:
    """The sequence owner_ref of one improve loop: the loop directory, exactly
    as the plan's sequence_ref contract names it ("owner_ref 指向该循环目录")."""
    return str(improve_command._loops_dir(session_id, cfg) / str(loop_id))


def improve_child_request_id(loop_id: int) -> str:
    """The stable create request_id of one improve loop's worker child."""
    return f"improve:{loop_id}"


def iteration_run_request_id(loop_id: int, iteration: int) -> str:
    """The stable run request_id of one iteration: a replayed finalization
    binds to the same Run, never a duplicate."""
    return f"improve:{loop_id}:iter:{iteration}"


async def create_improve_child(
    tree: TaskTreeManager,
    session_id: str,
    loop_id: int,
    goal: str,
    *,
    repo_path: str | None,
    base_branch: str | None,
) -> SessionMetadata:
    """Create (or re-admit) the loop's one worker child under the manager.

    Stable by request id: a replayed admission returns the original child. The
    child carries the loop goal as its task text and no task_type: the improve
    loop is its own deliverable kind, and the implement delivery policy
    (review + landing) is the delegate path's contract, not this one.
    """
    return await tree.create_task(
        request_id=improve_child_request_id(loop_id),
        task_parent_id=session_id,
        profile="worker",
        task=TaskSpec(
            goal=goal,
            repo_path=repo_path,
            base_branch=base_branch,
            task_type=None,
        ),
        name=f"Improve loop {loop_id}",
        backend=None,
        caller="operator",
    )


def _compose_iteration_description(goal: str, plan: str | None, previous_summaries: list[str]) -> str:
    """The iteration description, composed exactly as the legacy controller's."""
    parts = [goal]
    if plan is not None:
        parts.append(f"Plan:\n{plan}")
    if previous_summaries:
        parts.append("Previous iteration summaries:\n" + "\n\n".join(previous_summaries))
    return "\n".join(parts)


async def register_iteration_run(
    tree: TaskTreeManager,
    child_id: str,
    session_id: str,
    loop_id: int,
    iteration: int,
    cfg: CharlieBotConfig,
    *,
    resolved_backend: str,
    resolved_model: str | None,
    repo_path: str,
    base_branch: str,
    work_branch: str,
    worktree_path: str,
) -> RunRecord:
    """Register one iteration Run with its full shared-worktree provenance.

    The loop's single worktree, branch and base are pinned on the Run before
    launch, so the adapter never creates a worktree of its own and every
    iteration commits to the same branch the controller merges back. Stable by
    (child, request id): a replayed registration returns the original Run.
    """
    run_id = stable_run_id(child_id, iteration_run_request_id(loop_id, iteration))
    existing = await tree.runs.get_run(child_id, run_id)
    if existing is not None:
        return existing
    record = RunRecord(
        id=run_id,
        session_id=child_id,
        kind="iteration",
        backend=resolved_backend,
        model=resolved_model,
        repo_path=repo_path,
        base_branch=base_branch,
        branch_name=work_branch,
        worktree_path=worktree_path,
        sequence_ref=SequenceRef(
            kind="improve",
            owner_ref=loop_owner_ref(session_id, loop_id, cfg),
            position=iteration,
        ),
    )
    async with tree.control_lock:
        await tree.runs.register_run_locked(record, task_spec_text=None)
    fresh = await tree.runs.get_run(child_id, run_id)
    assert fresh is not None
    return fresh


async def terminal_outcome(tree: TaskTreeManager, child_id: str, run_id: str) -> str | None:
    run = await tree.runs.get_run(child_id, run_id)
    if run is None:
        return None
    return tree.runs.terminal_outcome(tree.runs.load_events_sync(child_id), run_id)


async def _iteration_blocker(
    tree: TaskTreeManager, child_id: str, run_id: str, iteration: int, outcome: str,
) -> tuple[str | None, str]:
    """The failed iteration's quota blocker and summary, from its own event log.

    Reuses the legacy mechanical judgments over the Run's translated events —
    the same quota-shaped-event definition, no new matcher.
    """
    events_path = tree.runs.run_dir(child_id, run_id) / "events.jsonl"
    if not events_path.is_file():
        return None, f"Iteration {iteration} {outcome} (no events log)."
    blocker_reason, summary = await asyncio.to_thread(
        improve_command._failed_iteration_judgments,
        improve_command._newest_first_events(events_path), iteration, outcome)
    return blocker_reason, summary


async def run_improve_sequence(
    session_id: str,
    cfg: CharlieBotConfig,
    tree: TaskTreeManager,
    *,
    loop_id: int,
    iterations: int,
    child_id: str,
    goal: str,
) -> None:
    """Run the improve sequence on the v2 child (the background controller task).

    The child and the loop state already exist (the API handler reserved both
    under the stable ids). Each iteration launches through the shared adapter,
    is awaited to its terminal fact, and is judged by the existing mechanical
    rules; the loop ends on exhaustion, user stop, or a quota blocker, and the
    ONE final result is delivered to the parent through the report owner.
    """
    from src.core.task_execution import TaskExecutionAdapter

    state = await improve_command.require_loop_state(session_id, loop_id, cfg)
    loop_dir = cfg.sessions_dir / session_id / "loops" / str(loop_id)
    resolved_repo = Path(state.repo_path)
    work_branch = state.work_branch
    base_branch = state.base_branch
    merge_back = state.merge_back
    resolved_backend = state.backend or ""
    resolved_model = state.model
    adapter = tree.dispatch.executor
    if not isinstance(adapter, TaskExecutionAdapter):
        raise RuntimeError("task execution adapter is not installed; cannot run an improve sequence")

    previous_summaries: list[str] = []
    blocked: tuple[int, str, str] | None = None  # (iteration, reason, summary)
    completed_iterations = 0

    try:
        # The single shared worktree every iteration commits to. Created here
        # (the controller owns it) and pinned on each iteration Run.
        wt_path = Path(cfg.paths.worktree_dir) / work_branch.replace('/', '-')
        Path(cfg.paths.worktree_dir).mkdir(parents=True, exist_ok=True)
        try:
            resolution = await git.git_create_worktree(
                resolved_repo, base_branch or await git.git_current_branch(resolved_repo), work_branch, wt_path)
            state.base_branch = resolution.canonical
            await improve_command.save_loop_state(session_id, state, cfg)
            base_branch = resolution.canonical
        except Exception as e:
            state.status = 'failed'
            await improve_command.save_loop_state(session_id, state, cfg)
            await improve_command.clear_active_loop_lock(session_id, cfg)
            log.error("improve_sequence_worktree_failed", session=session_id, loop_id=loop_id, error=str(e))
            await tree.sessions.deliver_to_successor(
                session_id, {"type": ET.IMPROVE_FAILED, "goal": goal,
                             "error": f"Failed to create worktree: {e}"})
            await _deliver_sequence_report(
                tree, child_id, session_id, loop_id, "failed",
                f"Improve loop failed to create its worktree: {e}", [])
            return

        for i in range(1, iterations + 1):
            state = await improve_command.require_loop_state(session_id, loop_id, cfg)
            if state.status == "stopped":
                break
            # The live goal (and optional plan) is re-read every iteration so a
            # mid-loop edit steers the next one; a missing goal.md fails loudly.
            goal = await improve_command.read_loop_goal(loop_dir)
            plan = await improve_command.read_loop_plan(loop_dir)
            description = _compose_iteration_description(goal, plan, previous_summaries)
            tip_before = await git._git_rev_parse(wt_path, "HEAD") or ""

            run = await register_iteration_run(
                tree, child_id, session_id, loop_id, i, cfg,
                resolved_backend=resolved_backend, resolved_model=resolved_model,
                repo_path=str(resolved_repo), base_branch=base_branch,
                work_branch=work_branch, worktree_path=str(wt_path))
            outcome = await terminal_outcome(tree, child_id, run.id)
            if outcome is None:
                # This controller's work: launch the iteration with its
                # composed description and await the launch/wait settlement.
                # (A replayed registration of an already-terminal Run keeps
                # its recorded outcome above; a live process from a replayed
                # registration is followed, never relaunched.)
                observation = await adapter.launch_and_settle(child_id, run.id, prompt=description)
                if observation.withheld is not None:
                    # No process started and no terminal fact will arrive: the
                    # loop cannot continue. Settle honestly — blocked state,
                    # released active lock, the actual reason reported — and
                    # leave the queued iteration Run standing as the retained
                    # pending request (resume/retry uses the existing policy;
                    # the whole loop is never automatically restarted).
                    await _settle_withheld_iteration(
                        tree, session_id, cfg, loop_id, child_id, goal, i, run.id,
                        observation.withheld, previous_summaries, iterations)
                    return
                outcome = observation.outcome
                assert outcome is not None
            completed_iterations = i

            if outcome != "success":
                blocker_reason, failed_summary = await _iteration_blocker(
                    tree, child_id, run.id, i, outcome)
                if blocker_reason:
                    log.warning("improve_sequence_blocked", session=session_id, loop_id=loop_id,
                                iteration=i, reason=blocker_reason)
                    blocked = (i, blocker_reason, failed_summary)
                    break
                # A non-quota failure is recorded and the loop continues, as
                # the legacy controller did.

            summary = await _judge_iteration(
                tree, child_id, run.id, i, wt_path, loop_dir, tip_before)
            previous_summaries.append(summary)
            await _broadcast_iteration_progress(
                tree, session_id, child_id, run.id, i, iterations, outcome, summary, loop_dir)

        state = await improve_command.require_loop_state(session_id, loop_id, cfg)
        stopped_by_user = state.status == 'stopped'

        merge_result: dict | None = None
        if blocked is None and not stopped_by_user:
            merge_result = await improve_command._land_work_branch_after_loop(
                resolved_repo, work_branch, base_branch, merge_back, stopped_by_user,
                previous_summaries, session_id)

        if blocked is not None:
            state.status = 'failed'
            outcome_label = "failed"
            summary = (
                f"Improve loop blocked on iteration {blocked[0]}: {blocked[1]}. "
                "No further iterations were spawned; decide whether to wait, switch backend, or relaunch.")
        elif stopped_by_user:
            state.status = 'stopped'
            outcome_label = "cancelled"
            summary = (
                f"Improve loop stopped by user after {completed_iterations} iteration(s); "
                "evidence retained on the loop's task, no further iterations will run.")
        elif merge_result is not None and merge_result.get('merged') is True:
            state.status = 'completed'
            outcome_label = "completed"
            summary = (
                f"Improve loop completed {completed_iterations} iteration(s); work branch "
                f"{work_branch} landed on {base_branch}.")
        else:
            # Exhausted iterations without proven delivery: never a fabricated
            # success. The child stays open; the parent decides what is next.
            state.status = 'blocked'
            outcome_label = "blocked"
            detail = ""
            if merge_result is not None and merge_result.get('merged') is False:
                detail = (f" The fast-forward landing onto {base_branch} failed "
                          f"({merge_result.get('error')}); the work branch was pushed to origin.")
            summary = (
                f"Improve loop ran {completed_iterations} iteration(s) on branch {work_branch}; "
                f"the goal is not proven and the sequence delivered no landing.{detail} "
                "Decide whether to land the branch, continue iterating, or close the task.")
        await improve_command.save_loop_state(session_id, state, cfg)
        await improve_command.clear_active_loop_lock(session_id, cfg)

        payload = improve_command._build_summary_payload(
            ET.IMPROVE_COMPLETED if outcome_label == "completed"
            else ET.IMPROVE_STOPPED if outcome_label == "cancelled"
            else ET.IMPROVE_FAILED, goal, previous_summaries)
        if blocked is not None:
            payload['blocked_iteration'] = blocked[0]
            payload['reason'] = blocked[1]
            payload['blocked_summary'] = blocked[2][:500]
        payload['work_branch'] = work_branch
        payload['base_branch'] = base_branch
        if merge_result is not None:
            payload['merge_result'] = merge_result
        await tree.sessions.deliver_to_successor(session_id, payload)

        await _deliver_sequence_report(
            tree, child_id, session_id, loop_id, outcome_label, summary, previous_summaries)
    except asyncio.CancelledError:
        log.warning("improve_sequence_cancelled", session=session_id, loop_id=loop_id)
        raise
    except Exception as exc:
        log.exception("improve_sequence_failed", session=session_id, loop_id=loop_id)
        state = await improve_command.load_loop_state(session_id, loop_id, cfg)
        if state is not None:
            state.status = 'failed'
            await improve_command.save_loop_state(session_id, state, cfg)
        await improve_command.clear_active_loop_lock(session_id, cfg)
        try:
            await tree.sessions.deliver_to_successor(session_id, {
                "type": ET.IMPROVE_FAILED,
                "goal": goal,
                "error": f"Improve loop failed: {exc}",
                "iterations_completed": completed_iterations,
            })
            await _deliver_sequence_report(
                tree, child_id, session_id, loop_id, "failed",
                f"Improve loop controller failed: {exc}", previous_summaries)
        except Exception:
            log.exception("improve_sequence_failure_report_failed", session=session_id, loop_id=loop_id)


async def _settle_withheld_iteration(
    tree: TaskTreeManager,
    session_id: str,
    cfg: CharlieBotConfig,
    loop_id: int,
    child_id: str,
    goal: str,
    iteration: int,
    run_id: str,
    reason: str,
    previous_summaries: list[str],
    iterations: int,
) -> None:
    """Settle a loop whose iteration launch was withheld (no terminal fact will arrive).

    The loop state cannot keep saying "running": the controller marks it
    blocked with the actual reason, releases the active lock, and delivers the
    ONE sequence report carrying that reason. No side effect ran and none is
    retried automatically; the queued iteration Run stays as the retained
    pending request for the existing explicit resume/retry policy.
    """
    state = await improve_command.require_loop_state(session_id, loop_id, cfg)
    state.status = "blocked"
    await improve_command.save_loop_state(session_id, state, cfg)
    await improve_command.clear_active_loop_lock(session_id, cfg)
    log.warning("improve_sequence_launch_withheld", session=session_id, loop_id=loop_id,
                iteration=iteration, run_id=run_id, reason=reason)
    summary = (
        f"Improve loop stopped before iteration {iteration}: its launch was withheld and no "
        f"process started ({reason}). Iteration run {run_id} stays queued on the loop's task as "
        "the retained pending request; no side effects ran and none will be retried "
        "automatically. Resume the withheld precondition and retry the run, or restart the "
        "loop explicitly.")
    payload = improve_command._build_summary_payload(ET.IMPROVE_FAILED, goal, previous_summaries)
    payload["blocked_iteration"] = iteration
    payload["reason"] = reason
    payload["withheld_run_id"] = run_id
    payload["iterations_requested"] = iterations
    await tree.sessions.deliver_to_successor(session_id, payload)
    await _deliver_sequence_report(
        tree, child_id, session_id, loop_id, "blocked", summary, previous_summaries)


async def _judge_iteration(
    tree: TaskTreeManager, child_id: str, run_id: str, iteration: int,
    wt_path: Path, loop_dir: Path, tip_before: str,
) -> str:
    """The mechanical iteration judgment: report validity over the git delta.

    Same rules as the legacy controller (report file + commit count over the
    worktree tip this iteration started from), sourced from the Run's shared
    worktree and the loop's report file.
    """
    report_path = loop_dir / f'iter_{iteration:04d}.md'
    if not await asyncio.to_thread(report_path.exists):
        # The worker wrote no report: fall back to its own closing words, as
        # the legacy controller did, and leave the same marked fallback file.
        events_path = tree.runs.run_dir(child_id, run_id) / "events.jsonl"
        fallback = f"Iteration {iteration} finished without a report file."
        if events_path.is_file():
            text = await asyncio.to_thread(
                improve_command._extract_iteration_summary,
                improve_command._newest_first_events(events_path), iteration, "finished")
            if text:
                fallback = text
        await asyncio.to_thread(
            report_path.write_text, "<!-- runner fallback: worker wrote no report -->\n" + fallback)
        return fallback
    tip_after, commits_added, diffstat = await improve_command._worktree_commit_delta(wt_path, tip_before)
    del diffstat
    report_valid, invalid_reason = await improve_command._iter_report_validity(
        report_path, iteration, commits_added)
    if report_valid:
        return (await asyncio.to_thread(report_path.read_text))[:500]
    return improve_command._invalid_iteration_summary(
        iteration, invalid_reason, commits_added, tip_before, tip_after, report_path)


async def _broadcast_iteration_progress(
    tree: TaskTreeManager, session_id: str, child_id: str, run_id: str,
    iteration: int, iterations: int, status: str, summary: str, loop_dir: Path,
) -> None:
    """The per-iteration progress event: chat visibility only, never an input.

    The v2 loop has no per-iteration master wake — the manager sees each
    result as chat history and receives the one final report as input.
    """
    report_path = loop_dir / f'iter_{iteration:04d}.md'
    await tree.sessions.deliver_to_successor(session_id, {
        "type": ET.IMPROVE_ITERATION_COMPLETED,
        "iteration": iteration,
        "total_iterations": iterations,
        "status": status,
        "summary": summary[:200],
        "report_path": str(report_path),
        "child_session_id": child_id,
        "run_id": run_id,
    })


async def _deliver_sequence_report(
    tree: TaskTreeManager,
    child_id: str,
    session_id: str,
    loop_id: int,
    outcome: str,
    summary: str,
    previous_summaries: list[str],
) -> None:
    """The ONE final sequence result, delivered through the common report owner.

    The source event is the child's latest durable run fact (its creation fact
    when the loop ended before any iteration), so the stable report id dedups
    repeated finalization and recovery passes. The child itself stays open:
    its evidence (the iteration Runs and loop reports) remains, and the parent
    — or the operator — closes it through the common closure guards.
    """
    meta = await tree.load_meta(child_id)
    if meta is None or not meta.task_parent_id:
        log.warning("improve_sequence_report_no_parent", session=session_id, child=child_id)
        return
    events = tree.fact_history(child_id)
    source = next(
        (e for e in reversed(events)
         if e.get("type") in (ET.RUN_FINISHED, ET.TASK_CREATED)), None)
    if source is None:
        raise RuntimeError(f"improve child {child_id} has no durable fact to source its report from")
    detail = ("\n\nIteration summaries:\n" + "\n\n".join(previous_summaries)) if previous_summaries else ""
    await tree.dispatch.deliver_child_report(
        child_id,
        source_event=source,
        outcome=outcome,
        summary=f"[Improve loop {loop_id}] {summary}{detail}",
        result_refs=[f"loop:{loop_id}"],
        recipient=meta.task_parent_id,
    )
    log.info("improve_sequence_report_delivered", session=session_id, child=child_id,
             loop_id=loop_id, outcome=outcome)


# ---------------------------------------------------------------------------
# Interrupted controllers (the restart boundary)
# ---------------------------------------------------------------------------


async def reconcile_interrupted_sequences(
    cfg: CharlieBotConfig, tree: TaskTreeManager, boot_pid: int | None = None,
) -> int:
    """Mark every improve loop whose controller died with the old process.

    The loop CONTINUATION is an explicit non-goal (the existing improve
    boundary): a restart never resumes the loop. What recovery owes is
    honesty — the state file cannot keep saying "running" with no controller,
    the active lock cannot block the next loop forever, and the chat stream
    carries one visible notice. The launched iteration itself is reconciled by
    the Run recovery pass, not here.

    A loop whose state says running under THIS process's pid has a live
    controller and is left alone.
    """
    pid = boot_pid if boot_pid is not None else os.getpid()
    repaired = 0
    sessions_dir = cfg.sessions_dir
    if not sessions_dir.is_dir():
        return 0
    for session_dir in sorted(sessions_dir.iterdir()):
        if not session_dir.is_dir():
            continue
        loops_dir = session_dir / "loops"
        if not loops_dir.is_dir():
            continue
        session_id = session_dir.name
        for loop_id in await asyncio.to_thread(improve_command._find_state_loop_ids_sync, loops_dir):
            state = await improve_command.load_loop_state(session_id, loop_id, cfg)
            if state is None or state.status != "running":
                continue
            if state.server_pid == pid:
                continue  # this process's own controller is alive
            state.status = "interrupted"
            await improve_command.save_loop_state(session_id, state, cfg)
            active = improve_command._active_loop_path(session_id, cfg)
            if await asyncio.to_thread(active.exists):
                await asyncio.to_thread(active.unlink)
            repaired += 1
            log.warning("improve_sequence_interrupted", session=session_id, loop_id=loop_id,
                        recorded_pid=state.server_pid)
            meta = await tree.load_meta(session_id)
            if meta is not None:
                await tree.sessions.deliver_to_successor(session_id, {
                    "type": ET.IMPROVE_FAILED,
                    "goal": state.goal,
                    "error": (
                        f"Improve loop {loop_id} was interrupted by a server restart; the loop is "
                        "NOT resumed automatically (the existing improve boundary). Its state is "
                        "marked interrupted, the stale lock is cleared, and the launched "
                        "iteration's terminal fact was reconciled. Restart the loop explicitly "
                        "if you want it to continue."),
                })
    return repaired
