"""The cron package's implementation of the runtime sequence-controller hook."""

from __future__ import annotations

import asyncio
import dataclasses
import datetime
import zoneinfo
from typing import TYPE_CHECKING

from src.features.cron import cron_files, cron_sequence, loader, scheduler
from src.infra import config, log_once, metadata_slots
from src.infra import event_types as ET
from src.runtime import session_anchors, session_lifecycle
from src.runtime.hooks import sequence_controllers

if TYPE_CHECKING:
  from collections.abc import Iterable

  from src.infra import models
  from src.runtime import task_sessions

log = log_once.LazyStructlogLogger()

_ROW_SCHEDULE_MEMO: tuple[object, tuple[str, ...], dict[str, dict], datetime.datetime] | None = None
_ROW_SCHEDULE_NO_FIRE = datetime.datetime.max.replace(tzinfo=datetime.UTC)


def next_run_iso(cron_expr: str, timezone: str) -> str:
  """ISO next fire time of *cron_expr* in *timezone*, computed from the wall clock on every call."""
  if "croniter" not in globals():
    scheduler.load_croniter(globals())
  tz = zoneinfo.ZoneInfo(timezone)
  now = datetime.datetime.now(tz)
  return croniter(cron_expr, now).get_next(datetime.datetime).isoformat()  # noqa: F821  # bound by load_croniter


def scheduled_report_prefix(task_name: str) -> str:
  """The fixed wrapper a firing's report carries on the bound node's fresh turn."""
  return (
      f"[Auto-triggered scheduled task result for '{task_name}']\n"
      "Review the worker/reviewer results below. Check: was the branch merged? "
      "Are there errors? Summarize the outcome.\n\n")


def _last_saturday_1am_utc(now: datetime.datetime) -> datetime.datetime:
  """The most recent Saturday 01:00 America/Los_Angeles, in UTC.

  Inside Saturday 00:00-00:59 PT that grid point is the coming 01:00, still
  ahead of *now*. The caller's window check (started before the boundary,
  boundary already past) drops a future boundary, so the recycle waits for
  the first wake after 01:00.
  """
  now_pt = now.astimezone(zoneinfo.ZoneInfo(config.HOUSE_TIMEZONE))
  days_since_sat = (now_pt.weekday() - 5) % 7
  last_sat_1am_pt = now_pt.replace(hour=1, minute=0, second=0, microsecond=0) - datetime.timedelta(days=days_since_sat)
  return last_sat_1am_pt.astimezone(datetime.UTC)


@dataclasses.dataclass(frozen=True)
class CronBinding:
  """The loaded cron tasks bound to a task-tree node: ``name`` is the first, ``names`` every one."""

  name: str
  names: tuple[str, ...]
  dedicated_backend: bool = True

  def backend_lock_detail(self, target_backend: str) -> str:
    return (
        f"backend '{target_backend}' cannot be switched to in place: this session is the bound node "
        f"of scheduled task '{self.name}', whose cron config decides its backend and whose "
        "scheduler re-aligns the node to that config on every tick. Edit the task's config in the "
        "cron editor, or clone/fork the session with the target backend instead.")

  async def on_wake(self, meta: models.SessionMetadata, input_events: list[dict]) -> str | None:
    """Recycle old history and prefix a firing report on the node's fresh turn."""
    if meta.cc_session_id and meta.cc_session_started_at:
      last_sat_1am_utc = _last_saturday_1am_utc(datetime.datetime.now(datetime.UTC))
      if meta.cc_session_started_at < last_sat_1am_utc < datetime.datetime.now(datetime.UTC):
        log.info('scheduled_cc_session_expired', session=meta.id, started_at=str(meta.cc_session_started_at))
        # The clear channel owns the disk write: anchors change only through
        # their authorized channels, and a plain whole-object save would be
        # corrected back to the old anchor by the save guard — the recycle
        # would silently do nothing behind its suppressed next-round alarm.
        # The in-memory copy mirrors the cleared anchor for the caller's
        # fresh-conversation judgment below.
        await session_anchors.anchors().clear_cc_session_anchor(meta.id)
        meta.cc_session_id = None
        meta.cc_session_started_at = None
        try:
          result = await session_lifecycle.lifecycle().recycle_history_before(meta.id, last_sat_1am_utc)
          log.info('scheduled_session_recycled', session=meta.id, **result)
        except Exception:
          log.exception('scheduled_session_recycle_failed', session=meta.id)

    woken_by_firing_report = any(e.get("type") == ET.CHILD_REPORT for e in input_events)
    if woken_by_firing_report and not meta.cc_session_id:
      return scheduled_report_prefix(self.name)
    return None

  async def on_archive(self) -> None:
    """Disable every task bound to the archived node: one left enabled errors on each scheduler tick."""
    for name in self.names:
      await asyncio.to_thread(cron_files.write_cron_key, name, "enabled", value=False)


class CronSequenceController(sequence_controllers.SequenceController):
  sequence_kind = "cron_steps"

  async def redrive(self, session_id: str, tree: task_sessions.TaskTreeManager, cfg: config.CharlieBotConfig) -> None:
    await cron_sequence.redrive_firing(session_id, tree, cfg)

  async def launch_context(
      self, meta: models.SessionMetadata, run: models.RunRecord, launch_text: str,
      cfg: config.CharlieBotConfig) -> str | None:
    """A step's context stays the runtime's rendering over the controller's prompt.

    The step prompt rides the launch verbatim (``launch_text``); the adapter
    renders it as the task/input context over the step Run's pinned
    worktree provenance, so this controller composes no context of its own.
    """
    return None

  async def after_run(
      self, session_id: str, run: models.RunRecord, tree: task_sessions.TaskTreeManager,
      cfg: config.CharlieBotConfig) -> None:
    """Re-drive the firing from this durable finish.

    The next permitted step launches, or the ONE boundary report re-delivers
    — without waiting for the next tick or restart, and idempotent against a
    live controller.
    """
    await self.redrive(session_id, tree, cfg)

  async def recover_run(
      self, session_id: str, run: models.RunRecord, outcome: str | None, tree: task_sessions.TaskTreeManager,
      cfg: config.CharlieBotConfig) -> bool:
    """Replay a terminal or registered-but-unlaunched step; True adds one follow-up.

    A terminal step re-drives the frontier (the next position launches or the
    ONE boundary report re-delivers); the same redrive replays an admitted
    step whose controller settled withheld or died before its launch. Both
    are idempotent by stable Run ids; a live process is followed, never
    relaunched.
    """
    if outcome is None and run.pid is not None:
      return False
    await self.redrive(session_id, tree, cfg)
    return True

  async def move_bindings(self, source_session_id: str, new_session_id: str) -> bool:
    """The elone move: every task bound to the source fires on its successor.

    The yaml's ``session_id`` key rebinds through the single-key write, one
    write per bound task; the loader's fingerprint picks the change up on the
    next read, so the next tick resolves the new node. True when at least one
    task was bound to the source.
    """
    names = cron_sequence.bound_task_names(source_session_id)
    for name in names:
      await asyncio.to_thread(cron_files.write_cron_key, name, "session_id", new_session_id)
      log.info("scheduled_task_moved", task=name, source=source_session_id, session=new_session_id)
    return bool(names)

  def binding(self, session_id: str) -> CronBinding | None:
    names = cron_sequence.bound_task_names(session_id)
    return CronBinding(names[0], tuple(names)) if names else None

  def owns_session(self, meta: models.SessionMetadata) -> bool:
    return metadata_slots.fields_of(meta, "cron").scheduled_task is not None

  def listing_fields(self, session_ids: Iterable[str], now_utc: datetime.datetime) -> dict[str, dict]:
    """The schedule payload per listed row, keyed on the loaded task binding.

    A bound node carries the same task and occurrence fields as before, while
    an unbound row carries ``schedule_task: null`` and no occurrence fields.
    One config snapshot feeds both the binding predicate and values. An
    unchanged id set and fingerprint reuse the stored map until its earliest
    served occurrence passes.
    """
    ids = tuple(sorted(set(session_ids)))
    global _ROW_SCHEDULE_MEMO
    hit = _ROW_SCHEDULE_MEMO
    tasks, fingerprint = loader.scheduled_tasks_snapshot()
    if (hit is not None and now_utc < hit[3] and hit[1] == ids and fingerprint == hit[0]):
      return hit[2]
    out: dict[str, dict] = {}

    for session_id in ids:
      task_name = cron_sequence.bound_task_name(session_id, tasks)
      if task_name is None:
        out[session_id] = {"schedule_task": None}
        continue
      # bound_task_name answered from this same snapshot, so the config exists.
      task = next(t for t in tasks if t.name == task_name)
      out[session_id] = {
          "schedule_task": task.name,
          "schedule_cron": task.cron,
          "schedule_timezone": task.timezone,
          "schedule_enabled": task.enabled,
          "schedule_next_run": next_run_iso(task.cron, task.timezone),
          "schedule_allow_failure": task.allow_failure,
      }
    fires = [
        datetime.datetime.fromisoformat(fields["schedule_next_run"])
        for fields in out.values()
        if fields["schedule_task"] is not None
    ]
    _ROW_SCHEDULE_MEMO = (fingerprint, ids, out, min(fires) if fires else _ROW_SCHEDULE_NO_FIRE)
    return out
