"""Follow-time silence recheck.

A mounted follow loop records the last raw-output moment (seeded from the raw
log's mtime, so pre-follow silence counts) and, on crossing the existing
NO_OUTPUT_REPORT_THRESHOLD, emits at most one reminder per mount via
``on_silence`` — the same text shape as the boot STALLED report. It never
judges death and never stops the follow.

The once-key (a boot-scoped set in init.py, keyed by thread_id) is shared by
the boot STALLED report and every mount's recheck: whichever emits first
claims the thread for this boot, so repeated re-mounts can never re-emit.
Nothing is persisted.
"""

from __future__ import annotations

import asyncio
import contextlib
import os
import time
from pathlib import Path

import pytest

from src.agents.backends.base import tail_follow_events
from src.core import init as init_module
from src.core.timeouts import NO_OUTPUT_REPORT_THRESHOLD


class _FakeSessionMgr:
  """Captures recovery reports instead of persisting them."""

  def __init__(self) -> None:
    self.events: list[dict] = []

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    self.events.append(event)

  async def deliver_to_successor(self, session_id: str, event: dict) -> str:
    await self.persist_and_broadcast(session_id, event)
    return session_id

  async def mark_unread(self, session_id: str) -> None:
    pass


@pytest.fixture(autouse=True)
def _clear_once_keys():
  init_module._silence_reported_thread_ids.clear()
  yield
  init_module._silence_reported_thread_ids.clear()


async def _consume(raw: Path, sink: list[dict], on_silence) -> None:
  # The sink must receive events as they arrive: the follow runs until
  # cancelled, so a collect-then-extend form would leave the sink empty, and
  # an async generator cannot feed list.extend directly.
  async for ev in tail_follow_events(
      raw,
      translate=lambda e: [e],
      is_alive=lambda: True,
      post_result_timeout=9999.0,
      poll_interval=0.05,
      on_silence=on_silence,
  ):
    sink.append(ev)


@pytest.mark.asyncio
async def test_silence_crossing_emits_exactly_one_recheck_and_follow_continues(tmp_path: Path) -> None:
  """Fake-clock crossing of the real 7200s threshold (raw mtime backdated to
  threshold−2s, so ~2s of real follow time crosses it): exactly one reminder,
  and the follow keeps consuming afterwards."""
  raw = tmp_path / "agent.raw.ndjson"
  raw.write_bytes(b"")
  margin = 2.0
  ts = time.time() - (NO_OUTPUT_REPORT_THRESHOLD - margin)
  os.utime(raw, (ts, ts))

  reports: list[str] = []
  events: list[dict] = []

  async def on_silence() -> None:
    reports.append("recheck")

  task = asyncio.create_task(_consume(raw, events, on_silence))
  try:
    # Cross the threshold; exactly one reminder for this mount, even after
    # more idle polling.
    deadline = time.monotonic() + margin + 3.0
    while not reports and time.monotonic() < deadline:
      await asyncio.sleep(0.05)
    assert reports == ["recheck"]
    await asyncio.sleep(0.5)
    assert reports == ["recheck"]

    # The follow never judged death and never stopped: a late line is consumed.
    with raw.open("ab") as f:
      f.write(b'{"type":"assistant"}\n')
    deadline = time.monotonic() + 2.0
    while not events and time.monotonic() < deadline:
      await asyncio.sleep(0.05)
    assert [e.get("type") for e in events] == ["assistant"]
  finally:
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task


@pytest.mark.asyncio
async def test_no_recheck_before_threshold(tmp_path: Path) -> None:
  """A freshly-written raw log is not silent: no reminder within a short window."""
  raw = tmp_path / "agent.raw.ndjson"
  raw.write_text('{"type":"assistant"}\n', encoding="utf-8")

  reports: list[str] = []
  events: list[dict] = []

  async def on_silence() -> None:
    reports.append("recheck")

  task = asyncio.create_task(_consume(raw, events, on_silence))
  try:
    await asyncio.sleep(0.5)
    assert not reports
    assert [e.get("type") for e in events] == ["assistant"]
  finally:
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task


@pytest.mark.asyncio
async def test_boot_report_and_recheck_share_the_once_key() -> None:
  """Whichever side emits first claims the thread for this boot; the other
  side (and any number of repeats) emits nothing more. Same channel and text
  shape as the boot STALLED report."""
  session_mgr = _FakeSessionMgr()

  # Boot STALLED report claimed the key first: this thread's mounts stay silent.
  init_module._silence_reported_thread_ids.add("thread-boot-reported")
  await init_module._follow_silence_recheck(session_mgr, "sess", "thread-boot-reported")
  assert not session_mgr.events

  # The recheck side claims first for a fresh thread; repeats cannot re-emit.
  await init_module._follow_silence_recheck(session_mgr, "sess", "thread-mounted")
  await init_module._follow_silence_recheck(session_mgr, "sess", "thread-mounted")
  assert len(session_mgr.events) == 1
  ev = session_mgr.events[0]
  assert ev["type"] == "error"
  assert ev["source"] == "crash_recovery"
  assert "still alive but produced no output" in ev["content"]
  assert "NOT being killed" in ev["content"]


@pytest.mark.asyncio
async def test_remount_cannot_reemit_within_one_boot(tmp_path: Path) -> None:
  """Repeated re-mounts of the same thread fire their per-mount recheck, but
  the boot-scoped once-key admits at most one report in total."""
  session_mgr = _FakeSessionMgr()
  raw = tmp_path / "agent.raw.ndjson"
  raw.write_text('{"type":"assistant"}\n', encoding="utf-8")
  ts = time.time() - (NO_OUTPUT_REPORT_THRESHOLD + 60)  # already past the threshold at mount
  os.utime(raw, (ts, ts))

  async def mount_once() -> None:
    events: list[dict] = []
    task = asyncio.create_task(
        _consume(raw, events, lambda: init_module._follow_silence_recheck(session_mgr, "sess", "tid")))
    await asyncio.sleep(0.4)
    task.cancel()
    with contextlib.suppress(asyncio.CancelledError):
      await task

  await mount_once()
  assert len(session_mgr.events) == 1
  await mount_once()
  await mount_once()
  assert len(session_mgr.events) == 1
