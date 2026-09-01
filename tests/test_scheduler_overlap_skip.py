"""Scheduler overlap-skip: at a due occurrence a scheduled fire is dropped while
the task's previous scheduled round is still in flight, without deferring it.

The criterion is the process-local in-flight handle (the ``asyncio.Task`` a
scheduled fire spawns via ``create_logged_task``). It reads nothing from disk
(neither thread ``status`` nor ``last_run_status``), so the thread-status
distortion and a stuck ``last_run_status`` never stall a task. Manual ``/run``
rounds record no handle and stay outside the judgment.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import AsyncMock

import pytest
from conftest import OPUS_BACKEND_ID, make_home_config

from src.core import scheduler as scheduler_module
from src.core.config import ScheduledTaskConfig
from src.core.models import (
  CreateSessionRequest,
  SessionMetadata,
  ThreadStatus,
  parse_utc_datetime,
)
from src.core.scheduler import Scheduler
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager


class _Clock:
  """Controllable ``datetime.now`` source for scheduler ticks."""

  def __init__(self, start: datetime) -> None:
    self._now = start

  def now(self, tz=None) -> datetime:
    if tz is None:
      return self._now
    return self._now.astimezone(tz)

  def set(self, value: datetime) -> None:
    self._now = value


class _FakeDate(datetime):
  """``datetime`` subclass whose ``now`` follows a controlled ``_Clock``.

  A subclass (not a bare replacement object) keeps
  ``croniter(...).get_next(datetime)`` on the real datetime path, so the due-time
  arithmetic stays byte-for-byte the production line.
  """

  _clock: _Clock

  @classmethod
  def now(cls, tz=None) -> datetime:
    return cls._clock.now(tz)


@dataclass
class _PendingRound:
  """A live in-flight asyncio round plus counting.

  ``fires`` counts executive births synchronously (set the moment the executor
  runs), so the no-catch-up and single-round assertions are exact and independent
  of whether the birthed round has yet been scheduled. ``started`` counts how
  many of those rounds have begun executing.
  """

  session: SessionMetadata
  fires: int = 0
  started: int = 0
  complete: asyncio.Event = field(default_factory=asyncio.Event)
  handle: asyncio.Task = None  # type: ignore[assignment]


def _install_clock(monkeypatch: pytest.MonkeyPatch, clock: _Clock) -> None:
  monkeypatch.setattr(
      scheduler_module,
      "datetime",
      type("_FakeDate", (_FakeDate,), {"_clock": clock}),
  )


def _task(name: str = "code-health", cron: str = "* * * * *", **kw) -> ScheduledTaskConfig:
  base: dict = {"name": name, "cron": cron, "timezone": "UTC", "prompt": "run the round"}
  base.update(kw)
  return ScheduledTaskConfig(**base)


def install_pending_executor(scheduler, clock, pending: _PendingRound):
  """Replace ``_execute_task`` with a fire-and-forget scheduled fire.

  Births a live pending asyncio round (registered as the in-flight handle when
  the scheduled path asks), stamps the fire-time anchor like the real execution
  path, and returns immediately so a tick never starts a second round itself.
  """

  async def _round() -> None:
    pending.started += 1
    await pending.complete.wait()

  async def _execute(task_cfg: ScheduledTaskConfig, record_handle: bool = False) -> dict:
    pending.fires += 1
    handle = asyncio.create_task(_round())
    pending.handle = handle
    if record_handle:
      scheduler._handles[task_cfg.name] = handle
    session = pending.session
    session.last_scheduled_run = clock.now(UTC).isoformat()
    return {"session_id": session.id, "thread_id": None}

  scheduler._execute_task = _execute


async def _tick(scheduler, task_cfg, session_mgr, clock, minute: int, second: int = 0) -> None:
  clock.set(datetime(2026, 6, 1, 0, minute, second, tzinfo=UTC))
  await scheduler._maybe_run(task_cfg, session_mgr, {}, None)


def _skip_events_since(session_mgr, since: int) -> int:
  """Count scheduled_run_skipped events emitted since ``since`` (len-based cursor)."""
  count = 0
  for call in session_mgr.persist_and_broadcast.await_args_list[since:]:
    e = call.args[1]
    if e.get("type") == "scheduled_run_skipped":
      count += 1
  return count


def _pending_rig(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> tuple[_Clock, Scheduler, SessionMetadata, AsyncMock, _PendingRound, ScheduledTaskConfig]:
  """Rig for the scheduled-path tests: clock parked at 2026-06-01 00:00 UTC, one
  pending in-flight round, session anchored at that instant, one-minute-cadence task."""
  clock = _Clock(datetime(2026, 6, 1, 0, 0, 0, tzinfo=UTC))
  _install_clock(monkeypatch, clock)
  scheduler = Scheduler(make_home_config(tmp_path), AsyncMock())
  session = SessionMetadata(id="session-1", name="Scheduled: code-health")
  session.last_scheduled_run = clock.now().isoformat()  # 00:00
  monkeypatch.setattr(scheduler, "_get_or_create_session", AsyncMock(return_value=session))
  session_mgr = AsyncMock()
  pending = _PendingRound(session)
  install_pending_executor(scheduler, clock, pending)
  task_cfg = _task()
  return clock, scheduler, session, session_mgr, pending, task_cfg


# ---------------------------------------------------------------------------
# 1. At most one round in flight
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_at_most_one_round_in_flight(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  clock, scheduler, _session, session_mgr, pending, task_cfg = _pending_rig(monkeypatch, tmp_path)

  await _tick(scheduler, task_cfg, session_mgr, clock, minute=1)
  await asyncio.sleep(0)  # let the birthed round become the in-flight handle
  for minute in (2, 3, 4, 5):
    await _tick(scheduler, task_cfg, session_mgr, clock, minute=minute)

  # Several due ticks arrived while the first round was still pending; the
  # pending round was never joined by a second one.
  assert pending.fires == 1
  pending.complete.set()
  await asyncio.sleep(0)


# ---------------------------------------------------------------------------
# 2. One judgment per due tick; the anchor advances through every occurrence
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_one_skip_per_due_tick_normal_cadence(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  """Normal one-minute tick cadence: one skip record per due occurrence."""
  clock, scheduler, session, session_mgr, pending, task_cfg = _pending_rig(monkeypatch, tmp_path)

  # Tick 1 fires the only round. Ticks 2..4 are each a due tick while pending.
  await _tick(scheduler, task_cfg, session_mgr, clock, minute=1)
  cursor = len(session_mgr.persist_and_broadcast.await_args_list)
  for minute in (2, 3, 4):
    await _tick(scheduler, task_cfg, session_mgr, clock, minute=minute)
    assert _skip_events_since(session_mgr, cursor) == 1, f"tick 00:{minute}"
    cursor = len(session_mgr.persist_and_broadcast.await_args_list)

  # The final skip (00:04) left the anchor at its own occurrence, nothing behind.
  assert parse_utc_datetime(session.last_scheduled_run) == datetime(
      2026, 6, 1, 0, 4, 0, tzinfo=UTC)
  pending.complete.set()
  await asyncio.sleep(0)


@pytest.mark.asyncio
async def test_one_skip_consuming_delayed_occurrences(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  """A tick delayed past several occurrences yields one record that consumes them."""
  clock, scheduler, session, session_mgr, pending, task_cfg = _pending_rig(monkeypatch, tmp_path)

  # One fire births the round; then a single delayed tick arrives at 00:04.
  await _tick(scheduler, task_cfg, session_mgr, clock, minute=1)
  cursor = len(session_mgr.persist_and_broadcast.await_args_list)
  await _tick(scheduler, task_cfg, session_mgr, clock, minute=4)

  # Exactly one record consumed occurrences 00:02, 00:03 and 00:04.
  assert _skip_events_since(session_mgr, cursor) == 1
  assert parse_utc_datetime(session.last_scheduled_run) == datetime(
      2026, 6, 1, 0, 4, 0, tzinfo=UTC)
  pending.complete.set()
  await asyncio.sleep(0)


# ---------------------------------------------------------------------------
# 3. No catch-up: the next fire is the next cron occurrence, not the completion
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_no_fire_on_completion_moment(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  """After the pending round finishes, further fires land on the cron grid, never
  at the completion moment. Failing the skip or firing on completion makes the
  mid-minute tick below birth a round, which this asserts does not happen."""
  clock, scheduler, session, session_mgr, pending, task_cfg = _pending_rig(monkeypatch, tmp_path)

  await _tick(scheduler, task_cfg, session_mgr, clock, minute=1)  # birth the round
  await _tick(scheduler, task_cfg, session_mgr, clock, minute=2)  # skip while pending

  # The pending round finishes; the tick immediately after completion lands at
  # a mid-minute moment (00:02:30), which is not on the cron grid.
  pending.complete.set()
  await asyncio.sleep(0)
  assert pending.handle.done()
  fires_at_completion = pending.fires
  await _tick(scheduler, task_cfg, session_mgr, clock, minute=2, second=30)

  # No fire happened at the completion moment: the executor never ran there, so
  # an implementation that refires on completion would trip this assertion.
  assert pending.fires == fires_at_completion

  # The next fire lands on 00:03:00, the next cron occurrence from the anchor
  # (00:02:00) that the last skip left.
  await _tick(scheduler, task_cfg, session_mgr, clock, minute=3)
  await asyncio.sleep(0)  # let the birthed round count its start
  assert pending.fires == fires_at_completion + 1
  assert parse_utc_datetime(session.last_scheduled_run) == datetime(
      2026, 6, 1, 0, 3, 0, tzinfo=UTC)


# ---------------------------------------------------------------------------
# 4. The criterion ignores disk state
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_fire_ignores_stuck_running_disk_state(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  """A session whose thread metadata says status=running with a pid that no
  longer exists, and whose last_run_status is stuck at 'running', still fires
  when no in-flight handle exists. Any implementation consulting either on-disk
  signal would stall this fire."""
  clock = _Clock(datetime(2026, 6, 1, 0, 0, 0, tzinfo=UTC))
  _install_clock(monkeypatch, clock)
  cfg = make_home_config(tmp_path)
  scheduler = Scheduler(cfg, AsyncMock())

  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(
      CreateSessionRequest(name="Scheduled: code-health", scheduled_task="code-health"),
      backend=OPUS_BACKEND_ID,
  )
  # A thread that is stuck running with a pid that no longer exists.
  thread_mgr = ThreadManager(cfg)
  thread = await thread_mgr.create_thread(session, "stuck round")
  await thread_mgr.update_status(session.id, thread.id, ThreadStatus.RUNNING, pid=999999)
  # The session bookkeeping is also stuck at running.
  session.last_scheduled_run = clock.now().isoformat()  # 00:00
  session.last_run_status = "running"
  await session_mgr.save_metadata(session)

  monkeypatch.setattr(scheduler, "_get_or_create_session", AsyncMock(return_value=session))
  fired = AsyncMock()
  monkeypatch.setattr(scheduler, "_execute_task", fired)
  task_cfg = _task()

  # No handle is in flight for this task (registry is empty), so despite the
  # running state on disk the due fire at 00:01 proceeds.
  session_mgr_double = AsyncMock()
  await _tick(scheduler, task_cfg, session_mgr_double, clock, minute=1)

  fired.assert_awaited()


# ---------------------------------------------------------------------------
# 5. Manual runs are outside the judgment
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_manual_run_is_outside_and_leaves_handle_unchanged(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  """run_task_now executes while a handle is in flight and leaves the recorded
  handle untouched (manual rounds neither block nor are blocked)."""
  clock = _Clock(datetime(2026, 6, 1, 0, 0, 0, tzinfo=UTC))
  _install_clock(monkeypatch, clock)
  cfg = make_home_config(tmp_path)
  scheduler = Scheduler(cfg, AsyncMock())

  # A scheduled round is in flight.
  async def _never() -> None:
    await asyncio.Event().wait()

  scheduled_handle = asyncio.create_task(_never())
  scheduler._handles["code-health"] = scheduled_handle

  monkeypatch.setattr(scheduler, "_execute_task", AsyncMock(return_value={"session_id": "s", "thread_id": "t"}))
  monkeypatch.setattr(scheduler_module, "get_config", lambda: cfg)
  monkeypatch.setattr(scheduler_module, "get_scheduled_tasks", lambda: [_task()])

  result = await scheduler.run_task_now("code-health")

  assert result == {"session_id": "s", "thread_id": "t"}
  # The manual execution left the recorded scheduled handle unchanged.
  assert scheduler._handles["code-health"] is scheduled_handle
  scheduled_handle.cancel()
  await asyncio.sleep(0)


# ---------------------------------------------------------------------------
# 6. Master mode is skipped by the same rule, not queued for a second wake
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_master_mode_skips_rather_than_queuing_a_second_wake(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  clock = _Clock(datetime(2026, 6, 1, 0, 0, 0, tzinfo=UTC))
  _install_clock(monkeypatch, clock)
  cfg = make_home_config(tmp_path)
  scheduler = Scheduler(cfg, AsyncMock())
  session = SessionMetadata(
      id="session-1",
      name="Scheduled: pm",
      role="project",
      group="bp-eval",
      scheduled_task="pm",
  )
  session.last_scheduled_run = clock.now().isoformat()  # 00:00
  monkeypatch.setattr(scheduler, "_get_or_create_session", AsyncMock(return_value=session))
  session_mgr = AsyncMock()
  task_cfg = _task("pm", mode="master", project="bp-eval", prompt="plan the day")

  woken = asyncio.Event()
  wake_count = {"n": 0}

  def fake_trigger_master(*_args, **_kwargs):
    wake_count["n"] += 1

    async def _wake() -> None:
      await woken.wait()

    return _wake()

  monkeypatch.setattr(scheduler_module, "trigger_master", fake_trigger_master)
  monkeypatch.setattr(scheduler_module, "create_logged_task", asyncio.create_task)
  monkeypatch.setattr(scheduler_module, "get_config", lambda: cfg)

  # First fire births the master wake and registers its in-flight handle.
  await _tick(scheduler, task_cfg, session_mgr, clock, minute=1)
  assert wake_count["n"] == 1
  handle = scheduler._handles.get(task_cfg.name)
  assert handle is not None and not handle.done()

  cursor = len(session_mgr.persist_and_broadcast.await_args_list)
  # A second due tick while the master wake is pending is skipped, not queued.
  await _tick(scheduler, task_cfg, session_mgr, clock, minute=2)
  assert wake_count["n"] == 1
  assert _skip_events_since(session_mgr, cursor) == 1

  woken.set()
  await asyncio.sleep(0)


# ---------------------------------------------------------------------------
# 7. Projection: the skip event renders as one system message
# ---------------------------------------------------------------------------


def test_skip_event_projects_to_single_system_message() -> None:
  from src.core.message_aggregator import MessageAggregator

  event = {
      "type": "scheduled_run_skipped",
      "task": "code-health",
      "skipped_at": "2026-06-01T00:02:00+00:00",
      "reason": "previous round still running (scheduled_worker_code-health_abc)",
  }
  aggregator = MessageAggregator()
  deltas = list(aggregator.feed(event))

  messages = [d for d in deltas if d.get("type") == "message"]
  assert len(messages) == 1
  msg = messages[0]["message"]
  assert msg["role"] == "system"
  assert "code-health" in msg["content"]
  assert "skipped" in msg["content"]
