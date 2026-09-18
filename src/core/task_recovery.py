"""The startup owner's v2 reconciliation pass: one store, one pass, no races.

This is the recovery the execution-adapter stage owes: at server start, BEFORE
any new chat input, cron fire, or delayed trigger can start a competing
process, every task-tree node owned by THIS configured instance is reconciled
from its own durable facts:

1. Interrupted sequence controllers are marked honestly (an improve loop whose
   controller died with the old process is never resumed — the existing
   boundary — but its state file, active lock and chat stream say so).
2. Every non-terminal Run converges: a durable stop request wins first; a
   live (pid, pid_start) process is re-attached and followed; an ended process
   drains to its durable result; definitely unstarted queued work re-enters
   through the same launch checks every fresh dispatch passes.
3. Terminal Runs replay their missing follow-up once, idempotently: the review
   chain, sequence advance, the parent report, and the owner-close recheck.
   A Run whose process ended before its follow-up ran is never skipped forever
   just because it already carries a terminal fact.
4. Persisted-but-undelivered parent reports are recovered, and each node's
   pending inputs dispatch — the empty-queued-reservation repair rides the
   deterministic pending-input identity (a replayed dispatch reclaims its
   batch without a new message).

Scans, identity checks, writes and cleanup stay inside this instance's
configured sessions directory and its owned records: another instance's data
and its processes are untouched. The v1 (thread/master_run) recovery keeps
serving unmigrated v1 sessions and never writes ThreadMetadata or master_run
for a v2 node.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.log_once import LazyStructlogLogger

if TYPE_CHECKING:
    from src.core.task_execution import TaskExecutionAdapter
    from src.core.task_sessions import SessionManager, TaskTreeManager

log = LazyStructlogLogger()

_FORKILL = "follow-up"


async def reconcile_task_tree(
    cfg: CharlieBotConfig,
    tree: TaskTreeManager,
    session_mgr: SessionManager,
    adapter: TaskExecutionAdapter | None = None,
) -> dict:
    """Reconcile every v2 node this instance owns. Returns pass counters."""
    from src.core.improve_sequence import reconcile_interrupted_sequences

    adapter = adapter if adapter is not None else tree.dispatch.executor
    counters = {"nodes": 0, "resumed": 0, "drained": 0, "followups": 0}
    if not cfg.sessions_dir.is_dir():
        return counters

    # The sequence controllers' honest verdicts come first: a recovered
    # "interrupted" improve state must not race the Run reconciliation below.
    await reconcile_interrupted_sequences(cfg, tree)

    for session_dir in sorted(cfg.sessions_dir.iterdir()):
        if not session_dir.is_dir():
            continue
        meta_path = session_dir / "metadata.json"
        if not meta_path.is_file():
            continue
        try:
            from src.core.json_utils import load_json_meta
            raw = load_json_meta(meta_path, "task_recovery_meta_unreadable")
        except Exception:
            log.exception("task_recovery_meta_read_failed", session=session_dir.name)
            continue
        if raw is None:
            continue
        if not raw.get("profile"):
            continue  # a v1 session: the legacy recovery owns it during the branch
        session_id = session_dir.name
        counters["nodes"] += 1
        try:
            await _reconcile_node(session_id, tree, session_mgr, adapter, counters, cfg)
        except Exception:
            log.exception("task_recovery_node_failed", session=session_id)
    return counters


async def _reconcile_node(
    session_id: str,
    tree: TaskTreeManager,
    session_mgr: SessionManager,
    adapter: TaskExecutionAdapter | None,
    counters: dict,
    cfg: CharlieBotConfig,
) -> None:
    meta = await tree.load_meta(session_id)
    if meta is None:
        return
    events = tree.runs.load_events_sync(session_id)
    run_records = tree.runs.list_run_records_sync(session_id)

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
        if tree.runs.run_has_terminal_fact(run, events):
            continue
        if tree.runs.stop_requested(events, run.id):
            # The owner's reconcile: a queued run never launches (it keeps the
            # request and claims nothing); a launched run is identity-checked,
            # signalled, and its observed exit lands as the terminal fact.
            await tree.runs.reconcile_stop_request(session_id, run.id)

    # --- 2. launched, non-terminal Runs re-attach or drain -----------------
    for run in run_records:
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
            from src.core.tasks import create_logged_task
            create_logged_task(adapter.resume_run(session_id, run.id),
                               name=f"task-resume-{run.id[:8]}")
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


async def _replay_cron_firing(
    session_id: str,
    tree: TaskTreeManager,
    run: object,
    cfg: CharlieBotConfig,
) -> None:
    """Re-drive one firing's chain/boundary through its owning module."""
    from src.core import cron_sequence

    seq = run.sequence_ref
    if seq is None or not seq.owner_ref.startswith("cron:"):
        return
    await cron_sequence.redrive_firing(session_id, tree, cfg)


async def _replay_followups(
    session_id: str,
    tree: TaskTreeManager,
    adapter: TaskExecutionAdapter | None,
    counters: dict,
    cfg: CharlieBotConfig,
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
    for run in run_records:
        outcome = tree.runs.terminal_outcome(events, run.id)
        if run.kind == "iteration":
            continue  # the improve loop is never resumed (the restart boundary)
        if run.kind == "scheduled_step":
            # The cron firing's chain advances from durable facts only — for a
            # terminal step the next position launches or the ONE boundary
            # report re-delivers; for a registered-but-unlaunched frontier step
            # (a controller that settled withheld or died before its launch)
            # the same redrive replays that admitted step. Both idempotent by
            # stable ids; a live process is followed, never relaunched.
            if outcome is not None or run.pid is None:
                counters["followups"] += 1
                await _replay_cron_firing(session_id, tree, run, cfg)
            continue
        if outcome is None:
            continue
        if meta.profile != "worker" and run.kind != "manager_turn":
            continue
        if meta.profile == "manager" and run.kind == "manager_turn":
            # The durable write happened, the crash landed before
            # after_run_finished: the owner-close recheck replays once (its
            # request-id replay dedups); phase 4's dispatch covers the rest.
            counters["followups"] += 1
            await tree.completion.recheck_close_requests(session_id, run.id)
            continue
        if run.kind == "work":
            counters["followups"] += 1
            if outcome == "success":
                # The completion owner's post-success follow-up replays exactly
                # as the dispatcher's finish path ran it: the own close-request
                # recheck and the non-implement automatic completion (close +
                # parent report). Everything inside is idempotent by stable
                # request/close/report ids, so a crash in the finish→follow-up
                # window is repaired and a repeated pass lands nothing twice.
                await tree.completion.after_run_finished(session_id, run.id)
                task_type = meta.task.task_type if meta.task is not None else None
                if task_type is not None and task_type == "implement" and run.repo_path:
                    await adapter._maybe_spawn_review(session_id, run)
            elif outcome in ("failed", "interrupted", "blocked"):
                await adapter._report_failure_to_parent(session_id, run, outcome)
        elif run.kind == "review":
            counters["followups"] += 1
            await adapter._after_review_run(meta, run, outcome)
