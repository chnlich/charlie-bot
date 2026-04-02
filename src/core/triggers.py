"""Delayed trigger (session self-wake) manager."""

import asyncio
from datetime import datetime, timedelta, timezone
from pathlib import Path

import aiofiles
import structlog

from src.api.message_utils import build_user_event
from src.core.config import CharlieBotConfig
from src.core.models import PendingTrigger, TriggerStatus
from src.core.sessions import SessionManager
from src.core.tasks import create_logged_task

log = structlog.get_logger()


class TriggerManager:
  """Manages delayed one-shot triggers that wake the master CC."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager):
    self._cfg = cfg
    self._session_mgr = session_mgr
    self._tasks: dict[str, asyncio.Task] = {}

  async def create_trigger(self, session_id: str, delay_seconds: int, message: str) -> PendingTrigger:
    """Create a pending trigger, persist to disk, and start the sleep task."""
    fire_at = datetime.now(timezone.utc) + timedelta(seconds=delay_seconds)
    trigger = PendingTrigger(session_id=session_id, fire_at=fire_at, message=message)
    await self._save_trigger(trigger)
    self._start_task(trigger)
    log.info("trigger_created", trigger_id=trigger.id, session=session_id, fire_at=fire_at.isoformat())
    return trigger

  async def list_triggers(self, session_id: str) -> list[PendingTrigger]:
    """Read all triggers for a session from disk."""
    triggers_dir = self._triggers_dir(session_id)
    if not triggers_dir.exists():
      return []
    files = await asyncio.to_thread(lambda: list(triggers_dir.glob("*.json")))
    triggers: list[PendingTrigger] = []
    for f in files:
      try:
        raw = await asyncio.to_thread(f.read_text, "utf-8")
        triggers.append(PendingTrigger.model_validate_json(raw))
      except Exception as e:
        log.warning("trigger_load_failed", path=str(f), error=str(e))
    triggers.sort(key=lambda t: t.created_at, reverse=True)
    return triggers

  async def cancel_trigger(self, session_id: str, trigger_id: str) -> None:
    """Mark a trigger as cancelled and cancel its asyncio task."""
    trigger = await self._load_trigger(session_id, trigger_id)
    if trigger.status != TriggerStatus.PENDING:
      return
    trigger.status = TriggerStatus.CANCELLED
    await self._save_trigger(trigger)
    task = self._tasks.pop(trigger_id, None)
    if task and not task.done():
      task.cancel()
    log.info("trigger_cancelled", trigger_id=trigger_id, session=session_id)

  async def recover_pending(self) -> None:
    """On startup, scan all sessions for pending triggers and restart their sleep tasks."""
    sessions_dir = self._cfg.sessions_dir
    if not sessions_dir.exists():
      return
    session_dirs = await asyncio.to_thread(lambda: [d for d in sessions_dir.iterdir() if d.is_dir()])
    for session_dir in session_dirs:
      triggers_dir = session_dir / "triggers"
      if not triggers_dir.exists():
        continue
      files = await asyncio.to_thread(lambda td=triggers_dir: list(td.glob("*.json")))
      for f in files:
        try:
          raw = await asyncio.to_thread(f.read_text, "utf-8")
          trigger = PendingTrigger.model_validate_json(raw)
        except Exception as e:
          log.warning("trigger_recovery_load_failed", path=str(f), error=str(e))
          continue
        if trigger.status == TriggerStatus.PENDING:
          self._start_task(trigger)
          log.info("trigger_recovered", trigger_id=trigger.id, session=trigger.session_id)

  def _start_task(self, trigger: PendingTrigger) -> None:
    """Start the asyncio sleep task for a trigger."""
    task = create_logged_task(self._wait_and_fire(trigger), name=f"trigger-{trigger.id[:8]}")
    self._tasks[trigger.id] = task

  async def _wait_and_fire(self, trigger: PendingTrigger) -> None:
    """Sleep until fire_at, then trigger the master agent."""
    now = datetime.now(timezone.utc)
    remaining = (trigger.fire_at - now).total_seconds()
    if remaining > 0:
      await asyncio.sleep(remaining)

    # Re-load to check for cancellation during sleep
    try:
      fresh = await self._load_trigger(trigger.session_id, trigger.id)
    except FileNotFoundError:
      log.warning("trigger_file_missing_after_sleep", trigger_id=trigger.id)
      return
    if fresh.status != TriggerStatus.PENDING:
      return

    trigger_message = f"[Scheduled trigger fired] {fresh.message}"
    await self._session_mgr.persist_and_broadcast(
        fresh.session_id,
        build_user_event(trigger_message),
    )

    # Wake the master CC
    from src.core.spawner import _trigger_master
    await _trigger_master(
      fresh.session_id,
      trigger_message,
      self._cfg,
      self._session_mgr,
    )

    fresh.status = TriggerStatus.FIRED
    fresh.fired_at = datetime.now(timezone.utc)
    await self._save_trigger(fresh)
    self._tasks.pop(trigger.id, None)
    log.info("trigger_fired", trigger_id=trigger.id, session=fresh.session_id)

  def _triggers_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id / "triggers"

  def _trigger_path(self, session_id: str, trigger_id: str) -> Path:
    return self._triggers_dir(session_id) / f"{trigger_id}.json"

  async def _save_trigger(self, trigger: PendingTrigger) -> None:
    path = self._trigger_path(trigger.session_id, trigger.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    async with aiofiles.open(path, "w") as f:
      await f.write(trigger.model_dump_json(indent=2))

  async def _load_trigger(self, session_id: str, trigger_id: str) -> PendingTrigger:
    path = self._trigger_path(session_id, trigger_id)
    async with aiofiles.open(path, "r") as f:
      raw = await f.read()
    return PendingTrigger.model_validate_json(raw)
