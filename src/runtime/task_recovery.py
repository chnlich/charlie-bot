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
and its processes are untouched.

This module walks the nodes and runs step 1. Steps 2 to 4 are the per-node pass
``task_execution._reconcile_node``, which the end-landing retry also calls.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from src.infra import config, log_once
from src.runtime import runs, task_execution

if TYPE_CHECKING:
  from src.runtime import task_sessions

log = log_once.LazyStructlogLogger()


async def reconcile_task_tree(
    cfg: config.CharlieBotConfig,
    tree: task_sessions.TaskTreeManager,
    adapter: task_execution.TaskExecutionAdapter | None = None,
) -> dict:
  """Reconcile every v2 node this instance owns. Returns pass counters."""
  adapter = adapter if adapter is not None else tree.dispatch.executor
  counters = {"nodes": 0, "resumed": 0, "drained": 0, "followups": 0}
  if not cfg.sessions_dir.is_dir():
    return counters

  # The sequence controllers' honest verdicts come first: a recovered
  # "interrupted" improve state must not race the Run reconciliation below.
  from src.runtime.hooks.sequence_controllers import sequence_controllers
  for controller in sequence_controllers():
    await controller.reconcile_interrupted(cfg, tree)

  for session_dir in sorted(cfg.sessions_dir.iterdir()):
    if not session_dir.is_dir():
      continue
    meta_path = session_dir / runs.METADATA_NAME
    if not meta_path.is_file():
      continue
    session_id = session_dir.name
    meta = await tree.load_meta(session_id)
    if meta is None:
      continue
    counters["nodes"] += 1
    try:
      await task_execution._reconcile_node(session_id, tree, adapter, counters, cfg)
    except Exception as exc:
      log.exception("task_recovery_node_failed", session=session_id)
      # An out-of-space node pass enters the end-landing retry: the
      # retry re-runs this pass until a round survives, so the node
      # converges once space returns (a restart would repair it the
      # same way — the retry just does it without waiting for one).
      if adapter is not None and task_execution.is_out_of_space_error(exc):
        adapter.start_run_end_landing_retry(session_id)
  return counters
