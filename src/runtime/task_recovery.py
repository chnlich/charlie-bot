"""The startup owner's v2 reconciliation pass: one store, one pass, no races.

This is the recovery the execution-adapter stage owes: at server start, BEFORE
any new chat input, scheduled fire, or delayed trigger can start a competing
process, every task-tree node owned by THIS configured instance is reconciled
from its own durable facts:

1. Interrupted sequence controllers are marked honestly (a loop whose
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

import collections
import datetime
import json
import os
import pathlib
from typing import TYPE_CHECKING

from src.infra import config, log_once, models
from src.runtime import runs, task_execution
from src.runtime.hooks import sequence_controllers

if TYPE_CHECKING:
  from src.runtime import task_sessions

log = log_once.LazyStructlogLogger()

MARKER_RELPATH = pathlib.Path("state") / "boot_recovery.json"


def marker_path(cfg: config.CharlieBotConfig) -> pathlib.Path:
  """The boot-recovery marker's location under this instance's home."""
  return cfg.charliebot_home / MARKER_RELPATH


def read_boot_recovery_marker(cfg: config.CharlieBotConfig) -> datetime.datetime | None:
  """The last clean pass's start time, or None when the marker is absent."""
  path = marker_path(cfg)
  try:
    payload = json.loads(path.read_text(encoding="utf-8"))
  except FileNotFoundError:
    return None
  return datetime.datetime.fromisoformat(payload["pass_started_at"])


def write_boot_recovery_marker(cfg: config.CharlieBotConfig, started_at: datetime.datetime) -> None:
  """Record the pass start time: fsync the tmp file, then rename it over the marker."""
  path = marker_path(cfg)
  path.parent.mkdir(parents=True, exist_ok=True)
  tmp = path.with_suffix(".json.tmp")
  with tmp.open("w", encoding="utf-8") as stream:
    stream.write(json.dumps({"pass_started_at": started_at.isoformat()}))
    stream.flush()
    # ext4 forces a file's data to disk before a rename only when the rename replaces an
    # existing file (auto_da_alloc), so a fresh marker can read back empty after a power loss.
    os.fsync(stream.fileno())
  os.replace(tmp, path)


def _cold_read_owes_delivery(
    meta: models.SessionMetadata, run_records: list[models.RunRecord], events: list[dict],
    run_store: runs.RunStore) -> bool:
  """Whether a cold-read task's last work Run failed with no retry Run following."""
  if meta.task is None or meta.task.task_type != models.TaskType.COLD_READ:
    return False
  work_runs = [r for r in run_records if r.kind == "work"]
  if not work_runs:
    return False
  return run_store.terminal_outcome(events, work_runs[-1].id) == "failed"


# A file's mtime comes from the kernel's coarse clock, while the marker holds a
# fine-clock reading: a write made just after the marker, or during the pass,
# can read up to about two clock ticks earlier than the pass start.
MARKER_MTIME_SLACK_SECONDS = 1.0


def _boot_skip_reason(
    tree: task_sessions.TaskTreeManager,
    session_id: str,
    session_dir: pathlib.Path,
    meta: models.SessionMetadata,
    marker: datetime.datetime,
) -> str | None:
  """The skip condition that failed, or None when the node is skippable.

    All six conditions must hold: the lifecycle state is not ``open``, both
    durable files predate the marker by more than the clock slack, every Run
    ended, no Run carries a sequence ref, no Run's worktree survives on disk,
    and a cold-read task's last work Run did not fail (its delivery chain would
    still owe a report).
    """
  if tree.task_state_fast(session_id) == "open":
    return "state_open"
  marker_ts = marker.timestamp()
  for path in (session_dir / runs.DATA_DIR_NAME / "chat_events.jsonl", session_dir / runs.METADATA_NAME):
    try:
      mtime = path.stat().st_mtime
    except FileNotFoundError:
      return "file_after_marker"
    if mtime >= marker_ts - MARKER_MTIME_SLACK_SECONDS:
      return "file_after_marker"
  run_records = tree.runs.list_run_records_sync(session_id)
  for run in run_records:
    if run.ended_at is None:
      return "run_unended"
    if run.sequence_ref is not None:
      return "sequence_ref"
    if run.worktree_path and os.path.lexists(run.worktree_path):
      return "worktree_exists"
  if meta.task is not None and meta.task.task_type == models.TaskType.COLD_READ:
    # Only a cold-read task's failed last work Run still owes a delivery;
    # other task types skip without ever loading the event history.
    events = tree.runs.load_events_sync(session_id)
    if _cold_read_owes_delivery(meta, run_records, events, tree.runs):
      return "cold_read_failed"
  return None


async def reconcile_task_tree(
    cfg: config.CharlieBotConfig,
    tree: task_sessions.TaskTreeManager,
    adapter: task_execution.TaskExecutionAdapter | None = None,
) -> dict:
  """Reconcile every v2 node this instance owns. Returns pass counters."""
  adapter = adapter if adapter is not None else tree.dispatch.executor
  counters = {"nodes": 0, "resumed": 0, "drained": 0, "followups": 0, "full": 0, "skipped": 0}
  if not cfg.sessions_dir.is_dir():
    return counters
  started_at = datetime.datetime.now(datetime.UTC)
  marker = read_boot_recovery_marker(cfg)
  keep_counts: collections.Counter[str] = collections.Counter()
  failed_node = False

  # The sequence controllers' honest verdicts come first: a recovered
  # "interrupted" sequence state must not race the Run reconciliation below.
  for controller in sequence_controllers.sequence_controllers():
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
    if marker is not None:
      reason = _boot_skip_reason(tree, session_id, session_dir, meta, marker)
      if reason is None:
        # The skip keeps the display warm-up: the sidebar row and header
        # badge show this node's newest Run's backend without a check.
        newest = tree.runs.newest_run_record_sync(session_id)
        if newest is not None:
          task_execution.thinking_state.note_run_backend(session_id, newest.backend)
        counters["skipped"] += 1
        continue
      keep_counts[reason] += 1
    try:
      counters["full"] += 1
      await task_execution._reconcile_node(session_id, tree, adapter, counters, cfg)
    except Exception as exc:
      log.exception("task_recovery_node_failed", session=session_id)
      # An out-of-space node pass enters the end-landing retry: the
      # retry re-runs this pass until a round survives, so the node
      # converges once space returns (a restart would repair it the
      # same way — the retry just does it without waiting for one).
      if adapter is not None and task_execution.is_out_of_space_error(exc):
        # The retry re-runs this pass until a round survives; the marker
        # only advances once a whole pass converges.
        adapter.start_run_end_landing_retry(session_id)
      failed_node = True
  if not failed_node:
    write_boot_recovery_marker(cfg, started_at)
  # The caller's task_tree_recovery_done line carries these: one line per pass.
  counters["marker"] = marker.isoformat() if marker else None
  counters["keep_reasons"] = dict(keep_counts)
  return counters
