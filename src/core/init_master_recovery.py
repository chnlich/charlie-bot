"""Master-side crash recovery plus the crash-recovery orchestrator."""

from __future__ import annotations

import asyncio
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING

from src.core import event_types as ET
from src.core import runs

if TYPE_CHECKING:
  from src.core.config import CharlieBotConfig
  from src.core.sessions import SessionManager

from src.core.init_worker_recovery import (
    _liveness_probe,
    _quarantine_stale_failed_worktrees,
    _report_recovery_event,
    _scan_thread_metas,
)
from src.core.log_once import LazyStructlogLogger
from src.core.tasks import create_logged_task

log = LazyStructlogLogger()


async def run_crash_recovery(
    cfg: CharlieBotConfig,
    boot_time: datetime,
    session_mgr: SessionManager | None = None,
    *,
    master_identity: asyncio.Task | None = None,
) -> None:
  """Master-side startup recovery plus the stale failed-worktree sweep.

  This is the deferrable part of startup: the server lifespan launches it as a
  background task so readiness is not delayed by the O(history) per-thread
  metadata scan. The synchronous scans run via ``asyncio.to_thread`` so they
  never block the event loop while live requests are served; the async git
  quarantine calls are awaited normally.

  Legacy threads are read-only records — nothing respawns them — so the
  thread scan here feeds only the stale failed-worktree quarantine. *boot_time* is
  captured at lifespan start (kept for signature stability and the quarantine
  window's anchor).

  *session_mgr* is injectable so the server passes its live instance;
  standalone callers (tests) get an auto-constructed one.

  Master-side reconciliation is the identity judgment
  (:func:`reconcile_master_identity`) plus the replay pass
  (``_replay_unanswered_inputs``): per session, a recorded master turn
  is resolved through ``runs.resolve_run``'s outcome table — re-attached or
  drained through the follower, or cleared — and every real user message
  after the last MASTER_DONE (minus the recorded turn's per-event exclusion)
  is replayed with a replay marker through the normal per-session queue.

  *master_identity*: the lifespan passes the one shielded identity task it
  already barriered on; this function awaits that same task (done or not)
  and runs the replay pass with its exclusion map. If the task raised, the
  exception is logged loudly and the replay pass is skipped entirely — a
  replay on a partial exclusion map double-answers turns — while the rest of
  startup continues. Standalone callers (tests) pass nothing and get both
  passes inline.
  """
  if session_mgr is None:
    from src.core.sessions import SessionManager
    session_mgr = SessionManager(cfg)
  # Legacy threads are read-only records: nothing respawns them. Startup
  # sweeps the failed worktrees the thread metadata names.
  threads = await asyncio.to_thread(_scan_thread_metas, cfg)
  if master_identity is not None:
    # The lifespan barrier may already have awaited this task; re-awaiting a
    # done task is free, and the barrier's shield keeps the one execution
    # running past a barrier timeout.
    try:
      excluded = await master_identity
    except Exception:
      log.exception("master_identity_failed")
    else:
      await _replay_unanswered_inputs(cfg, session_mgr, excluded)
  else:
    await _reconcile_master_runs(cfg, session_mgr, boot_time)
  await _quarantine_stale_failed_worktrees(cfg, threads)


def unanswered_input_events(chat_events: list[dict], exclude_ids: set[str]) -> list[dict]:
  """Real user events and scheduled-trigger wakes after the last MASTER_DONE, minus exclusions.

  Restart-replay contract: every replayable input event with no MASTER_DONE
  after it is unanswered and must be redelivered. Exclusions are per-event
  (never per-session): a recorded live turn excludes exactly its own
  user_event_ids, and a live process excludes the events already sitting in
  its in-memory queue — everything else disappears with a killed process and
  is replayed. A trigger wake whose record already left pending
  (FIRED-on-delivery) has no re-fire left in it, so the replay pass is the
  only thing that can redeliver it.
  """
  last_done = -1
  for idx, ev in enumerate(chat_events):
    if ev.get("type") == ET.MASTER_DONE:
      last_done = idx
  return [
      ev for ev in chat_events[last_done + 1:] if ev.get("type") in (
          ET.USER, ET.SCHEDULED_TRIGGER) and isinstance(ev.get("content"), str) and ev.get("id") not in exclude_ids
  ]


async def _await_reattach(future: asyncio.Future) -> None:
  """Await a re-attached master turn so its recovery task stays alive to the end."""
  await future


class _MasterScanFailedError(Exception):
  """The active-session listing at the head of the identity pass failed.

  A distinct type so callers can tell "no records were judged at all" apart
  from a mid-loop failure: the former skips the replay pass but lets the rest
  of startup continue (today's behavior), the latter propagates.
  """


def _master_alive_unfollowable_message(reason: str) -> str:
  """The one "alive but unfollowable" report for a master turn.

  Every "alive but unfollowable" branch reports the same outcome; only the
  parenthesised reason varies (an unresolved backend option, an uncovered
  transport, a missing raw log).
  """
  return (
      f"The recorded master turn on this session is treated as still alive ({reason}); "
      "it is NOT being killed or re-answered — left running without re-attach and judged "
      "again on the next restart.")


async def reconcile_master_identity(cfg: CharlieBotConfig, session_mgr: SessionManager,
                                    boot_time: datetime) -> dict[str, set[str]]:
  """Resolve each active session's recorded master turn; return the replay exclusion set.

  master_run is a single slot per session that a new turn's _on_spawn
  overwrites unconditionally, so this judgment must complete before any door
  that can create a new turn. The server lifespan therefore runs it once,
  shielded, behind a bounded barrier ahead of the crash-recovery task, the
  scheduler, and trigger recovery; the same task is re-awaited for the replay
  pass. Every follower-bound row adds its whole user_event_ids list to the
  returned map, so a turn (single input or merged batch) is answered by the
  drain OR by replay, never both.

  Each pre-boot record is resolved through ``runs.resolve_run``'s outcome
  table on disk facts, not a local alive/covered guess. COMPLETED goes
  through the follower to drain the bytes the dead producer left after the
  cursor and close the round; RUNNING/STALLED re-attach through the same
  follower; DIED without a final result is drained too when the turn has no
  user message to replay, or cleared when it has one (replay answers it);
  DIED on an uncovered backend transport or with a missing raw log, and
  NEVER_STARTED, clear the record. Records spawned by this same process
  during the recovery window (started_at >= boot_time) are spared.

  The returned map is ``excluded``: session id -> user event ids the replay
  pass must not re-answer. A session-listing failure raises
  ``_MasterScanFailedError``: no records were judged, so a replay pass run on the
  empty map would double-answer turns — every caller skips replay on it.
  """
  from src.agents import master_cc  # lazy: mirrors the spawner import's cycle guard

  try:
    sessions = await asyncio.to_thread(session_mgr.list_active_session_metas)
  except Exception as e:
    log.exception("master_reconcile_scan_failed")
    raise _MasterScanFailedError(str(e)) from e
  host_boot = await asyncio.to_thread(runs.read_host_boot_time)
  excluded: dict[str, set[str]] = {}

  for meta in sessions:
    record = meta.master_run
    if record is None:
      continue
    if record.started_at >= boot_time:
      # This process spawned the turn during the recovery window — live and
      # self-owned: spare it, and keep its inputs out of the replay set.
      excluded.setdefault(meta.id, set()).update(record.user_event_ids)
      continue
    option = cfg.get_backend_option(meta.backend)
    if option is None:
      if runs.is_run_alive(record.pid, record.pid_start, record.started_at, host_boot):
        # The translate and the transport judgment both need the session's
        # backend option, but the record proves the turn alive: same "alive
        # but unfollowable" route as the RUNNING branch below — report, keep
        # the record, keep the user message out of this boot's replay set.
        log.warning(
            "master_run_resolved",
            session=meta.id,
            pid=record.pid,
            outcome=None,
            reason=f"backend option {meta.backend!r} unresolved, record alive",
        )
        await _report_recovery_event(
            session_mgr, meta.id, _master_alive_unfollowable_message(f"backend option {meta.backend!r} unresolved"))
        excluded.setdefault(meta.id, set()).update(record.user_event_ids)
        continue
      # The translate and the transport judgment both need the session's
      # backend option; without it and with liveness unprovable the turn can
      # only be cleared, leaving its answerable inputs to the replay pass.
      log.warning(
          "master_run_resolved",
          session=meta.id,
          pid=record.pid,
          outcome=None,
          reason=f"backend option {meta.backend!r} unresolved",
      )
      await session_mgr.persist_master_run(meta.id, None)
      continue
    resolution = runs.resolve_run(
        raw_path=Path(record.raw_log),
        pid=record.pid,
        pid_start=record.pid_start,
        started_at=record.started_at,
        backend_type=option.type,
        translate=master_cc._build_fresh_translate(cfg, option),
        host_boot_time=host_boot,
    )
    log.info(
        "master_run_resolved",
        session=meta.id,
        pid=record.pid,
        outcome=resolution.outcome.value,
        reason=resolution.reason,
    )

    if (resolution.outcome is runs.RunOutcome.RUNNING and
        resolution.reason in (runs.UNCOVERED_ALIVE_REASON, runs.RAW_MISSING_ALIVE_REASON)):
      # Same rule as the worker branch: treated as alive but nothing
      # followable — report only. The record is kept (the turn still owns its
      # inputs until a real outcome lands) and they stay out of this boot's
      # replay set.
      await _report_recovery_event(session_mgr, meta.id, _master_alive_unfollowable_message(resolution.reason))
      excluded.setdefault(meta.id, set()).update(record.user_event_ids)
      continue

    follow = (
        resolution.outcome in (
            runs.RunOutcome.COMPLETED,
            runs.RunOutcome.RUNNING,
            runs.RunOutcome.STALLED,
        ) or (
            resolution.outcome is runs.RunOutcome.DIED and resolution.reason == runs.DIED_WITHOUT_RESULT_REASON and
            not record.user_event_ids))
    if follow:
      future = await master_cc.enqueue_master_resume(
          cfg,
          meta,
          record,
          session_mgr.callbacks(),
          is_alive=_liveness_probe(record.pid, record.pid_start, record.started_at, host_boot),
      )
      create_logged_task(_await_reattach(future), name=f"master-resume-{meta.id[:8]}")
      # The follower answers this round; keep its whole input list out of the
      # replay set so the round is answered exactly once.
      excluded.setdefault(meta.id, set()).update(record.user_event_ids)
    else:
      # DIED without a result but with answerable inputs (the replay pass
      # answers them), DIED on an uncovered transport or with a missing raw
      # log, or NEVER_STARTED: nothing drainable remains — clear the record.
      await session_mgr.persist_master_run(meta.id, None)

  return excluded


async def _replay_unanswered_inputs(
    cfg: CharlieBotConfig, session_mgr: SessionManager, excluded: dict[str, set[str]]) -> None:
  """Replay every input event the identity pass left unanswered.

  Every real user event and scheduled-trigger wake after the last MASTER_DONE,
  minus the identity pass's per-event exclusions and this process's queued
  items, is redelivered with the replay marker — user messages through
  ``replay_user_message``, wakes through ``replay_scheduled_trigger`` (a wake
  whose record already left pending cannot re-fire). Both declare their input
  type, so the replayed items batch into one following turn like any other
  queued input. This pass runs strictly after the identity pass enqueued the
  resume items, so a re-attached turn always drains before a replay spawns a
  new CLI against the same conversation. Never call it without the identity
  pass's full exclusion map: replaying with a partial map double-answers a
  turn.
  """
  from src.agents import master_cc  # lazy: mirrors the spawner import's cycle guard

  for meta in await asyncio.to_thread(session_mgr.list_active_session_metas):
    if meta.profile is not None:
      # A task-tree (v2) session's pending inputs are dispatcher facts the v2
      # recovery replays; a legacy replay here would double-admit them.
      continue
    try:
      events = session_mgr.load_chat_events_sync(meta.id)
      skip = excluded.get(meta.id, set()) | master_cc.queued_user_event_ids(meta.id)
      for ev in unanswered_input_events(events, skip):
        log.warning("master_replaying_user_message", session=meta.id, event_id=ev.get("id"))
        replay = (
            master_cc.replay_scheduled_trigger(cfg, meta, ev, session_mgr.callbacks()) if ev.get("type")
            == ET.SCHEDULED_TRIGGER else master_cc.replay_user_message(cfg, meta, ev, session_mgr.callbacks()))
        create_logged_task(replay, name=f"master-replay-{meta.id[:8]}")
    except Exception:
      log.exception("master_replay_dispatch_failed", session=meta.id)


async def _reconcile_master_runs(cfg: CharlieBotConfig, session_mgr: SessionManager, boot_time: datetime) -> None:
  """Thin wrapper kept for existing callers: identity pass, then replay pass.

  New code should run :func:`reconcile_master_identity` once (before any door
  that can create a new turn) and ``_replay_unanswered_inputs`` only
  with its returned exclusion map.
  """
  try:
    excluded = await reconcile_master_identity(cfg, session_mgr, boot_time)
  except _MasterScanFailedError:
    # Already logged: no records were judged, and replaying on an empty
    # exclusion map would double-answer turns — skip replay as before.
    return
  await _replay_unanswered_inputs(cfg, session_mgr, excluded)
