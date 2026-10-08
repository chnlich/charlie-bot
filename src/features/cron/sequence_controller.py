"""The cron package's implementation of the runtime sequence-controller hook."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING
from zoneinfo import ZoneInfo

from src.features.cron import loader
from src.features.cron.cron_files import write_cron_key
from src.infra import config, metadata_slots
from src.infra import event_types as ET
from src.infra.log_once import LazyStructlogLogger
from src.runtime import session_anchors, session_lifecycle
from src.runtime.hooks.sequence_controllers import SequenceController

if TYPE_CHECKING:
  from collections.abc import Iterable

  from src.infra.config import CharlieBotConfig
  from src.infra.models import RunRecord, SessionMetadata
  from src.runtime.task_sessions import TaskTreeManager

log = LazyStructlogLogger()

_ROW_SCHEDULE_MEMO: tuple[object, tuple[str, ...], dict[str, dict], datetime] | None = None
_ROW_SCHEDULE_NO_FIRE = datetime.max.replace(tzinfo=UTC)


def next_run_iso(cron_expr: str, timezone: str, now_utc: datetime) -> str:
  """ISO next fire time of *cron_expr* in *timezone*, computed on every call."""
  from src.features.cron.scheduler import load_croniter

  if "croniter" not in globals():
    load_croniter(globals())
  tz = ZoneInfo(timezone)
  now = datetime.now(tz)
  return croniter(cron_expr, now).get_next(datetime).isoformat()  # noqa: F821  # bound by load_croniter


def scheduled_report_prefix(task_name: str) -> str:
  """The fixed wrapper a firing's report carries on the bound node's fresh turn."""
  return (
      f"[Auto-triggered scheduled task result for '{task_name}']\n"
      "Review the worker/reviewer results below. Check: was the branch merged? "
      "Are there errors? Summarize the outcome.\n\n")


def _last_saturday_1am_utc(now: datetime) -> datetime:
  """The most recent Saturday 01:00 America/Los_Angeles, in UTC.

  Inside Saturday 00:00-00:59 PT that grid point is the coming 01:00, still
  ahead of *now*. The caller's window check (started before the boundary,
  boundary already past) drops a future boundary, so the recycle waits for
  the first wake after 01:00.
  """
  now_pt = now.astimezone(ZoneInfo(config.HOUSE_TIMEZONE))
  days_since_sat = (now_pt.weekday() - 5) % 7
  last_sat_1am_pt = now_pt.replace(hour=1, minute=0, second=0, microsecond=0) - timedelta(days=days_since_sat)
  return last_sat_1am_pt.astimezone(UTC)


@dataclass(frozen=True)
class CronBinding:
  """One loaded cron task bound to a task-tree node."""

  name: str
  dedicated_backend: bool = True

  def backend_lock_detail(self, target_backend: str) -> str:
    return (
        f"backend '{target_backend}' cannot be switched to in place: this session is the bound node "
        f"of scheduled task '{self.name}', whose cron config decides its backend and whose "
        "scheduler re-aligns the node to that config on every tick. Edit the task's config in the "
        "cron editor, or clone/fork the session with the target backend instead.")

  async def on_wake(self, meta: SessionMetadata, input_events: list[dict]) -> str | None:
    """Recycle old history and prefix a firing report on the node's fresh turn."""
    if meta.cc_session_id and meta.cc_session_started_at:
      last_sat_1am_utc = _last_saturday_1am_utc(datetime.now(UTC))
      if meta.cc_session_started_at < last_sat_1am_utc < datetime.now(UTC):
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
    await asyncio.to_thread(write_cron_key, self.name, "enabled", value=False)


class CronSequenceController(SequenceController):
  sequence_kind = "cron_steps"

  async def redrive(self, session_id: str, tree: TaskTreeManager, cfg: CharlieBotConfig) -> None:
    from src.features.cron.cron_sequence import redrive_firing

    await redrive_firing(session_id, tree, cfg)

  async def launch_context(
      self, meta: SessionMetadata, run: RunRecord, launch_text: str, cfg: CharlieBotConfig) -> str | None:
    """A step's context stays the runtime's rendering over the controller's prompt.

    The step prompt rides the launch verbatim (``launch_text``); the adapter
    renders it as the task/input context over the step Run's pinned
    worktree provenance, so this controller composes no context of its own.
    """
    return None

  async def after_run(self, session_id: str, run: RunRecord, tree: TaskTreeManager, cfg: CharlieBotConfig) -> None:
    """Re-drive the firing from this durable finish.

    The next permitted step launches, or the ONE boundary report re-delivers
    — without waiting for the next tick or restart, and idempotent against a
    live controller.
    """
    await self.redrive(session_id, tree, cfg)

  async def recover_run(
      self, session_id: str, run: RunRecord, outcome: str | None, tree: TaskTreeManager, cfg: CharlieBotConfig) -> bool:
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

  def binding(self, session_id: str) -> CronBinding | None:
    from src.features.cron.cron_sequence import bound_task_name

    name = bound_task_name(session_id)
    return None if name is None else CronBinding(name)

  def owns_session(self, meta: SessionMetadata) -> bool:
    return metadata_slots.fields_of(meta, "cron").scheduled_task is not None

  def listing_fields(self, session_ids: Iterable[str], now_utc: datetime) -> dict[str, dict]:
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
    from src.features.cron.cron_sequence import bound_task_name

    for session_id in ids:
      task_name = bound_task_name(session_id, tasks)
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
          "schedule_next_run": next_run_iso(task.cron, task.timezone, now_utc),
          "schedule_allow_failure": task.allow_failure,
      }
    fires = [
        datetime.fromisoformat(fields["schedule_next_run"])
        for fields in out.values()
        if fields["schedule_task"] is not None
    ]
    _ROW_SCHEDULE_MEMO = (fingerprint, ids, out, min(fires) if fires else _ROW_SCHEDULE_NO_FIRE)
    return out
