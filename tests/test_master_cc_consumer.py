"""Tests for master_cc._session_consumer cc_session_id relay and thinking_state ownership."""

from __future__ import annotations

import asyncio
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock

import pytest
from conftest import (
  BROADCAST_PATCH_TARGET,
  make_work_item,
  mock_session_callbacks,
  patch_instructions_content,
  run_session_consumer,
)
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents import master_cc, master_cc_queue, master_cc_run, master_cc_state
from src.agents.backends.base import make_result_event, make_text_event
from src.api import sessions as sessions_api
from src.api.deps import get_session_manager
from src.core import event_types as ET
from src.core import runs, thinking_state
from src.core import sessions as sessions_module
from src.core.config import CharlieBotConfig
from src.core.models import (
  BackendOption,
  CreateSessionRequest,
  MasterRunRecord,
  SessionCallbacks,
  SessionMetadata,
)
from src.core.sessions import SessionManager


def _make_meta(session_id: str) -> SessionMetadata:
  return SessionMetadata(id=session_id, name="t", backend="fake", cc_session_id=None)


@pytest.mark.asyncio
async def test_consumer_relays_cc_session_id_across_metadata_instances() -> None:
  """Two queued _WorkItems with distinct SessionMetadata objects must share cc_session_id.

  Reproduces the fork_session race where the bootstrap turn sets cc_session_id on
  meta_A but a concurrently-loaded meta_B still has cc_session_id=None.
  """
  session_id = "test-session-relay"

  meta_bootstrap = _make_meta(session_id)
  meta_user_message = _make_meta(session_id)  # distinct instance, freshly loaded from disk
  cb = mock_session_callbacks()
  item_bootstrap = make_work_item(MagicMock(), meta_bootstrap, None, user_content="hi", callbacks=cb)
  item_user = make_work_item(MagicMock(), meta_user_message, None, user_content="hi", callbacks=cb)

  observed_cc_session_ids: list = []

  async def fake_run_cc(item: master_cc._WorkItem):
    observed_cc_session_ids.append(item.session_meta.cc_session_id)
    return ("cc-id-from-bootstrap", 0, None, {})

  await run_session_consumer(session_id, [item_bootstrap, item_user], fake_run_cc)

  assert observed_cc_session_ids == [None, "cc-id-from-bootstrap"], (
      "second _run_cc must observe cc_session_id relayed from bootstrap meta")
  assert meta_user_message.cc_session_id == "cc-id-from-bootstrap"
  assert item_bootstrap.future.done() and item_bootstrap.future.result() == "cc-id-from-bootstrap"
  assert item_user.future.done() and item_user.future.result() == "cc-id-from-bootstrap"


# ---------------------------------------------------------------------------
# thinking_state: single in-memory owner of busy intervals (T1-T5)
# ---------------------------------------------------------------------------


def _make_consumer_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      backend_options=[BackendOption(id="fake", label="Fake", type="codex")],
  )


def _reset_master_state(session_id: str) -> None:
  master_cc_state._session_queues.pop(session_id, None)
  master_cc_state._session_consumers.pop(session_id, None)
  thinking_state.clear_busy(session_id)


@pytest.mark.asyncio
@pytest.mark.parametrize("inject_at", ["run_cc", "master_done_persist", "worker_probe", "idle_broadcast"])
async def test_busy_invariant_holds_under_adversarial_enqueue(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    inject_at: str,
) -> None:
  """T1: an enqueue landing at any await in the consumer's tail keeps the invariant.

  Parametrised over every await in the tail (the MASTER_DONE persist, the
  worker probe, the idle broadcast) and over _run_cc itself. At each injection
  point one extra work item is enqueued through the real run_message; every
  entry into _run_cc must observe busy_since non-None, and after the consumer
  ends busy_since must be None.
  """
  session_id = f"t1-{inject_at}"
  cfg = _make_consumer_cfg(tmp_path)
  monkeypatch.setattr(master_cc_queue, "get_tex_path", lambda: tmp_path / "missing.tex")

  entries: list[datetime | None] = []
  injected = False
  injected_task: asyncio.Task | None = None
  real_persist = AsyncMock()

  async def _inject_once() -> None:
    nonlocal injected, injected_task
    if injected:
      return
    injected = True
    injected_task = asyncio.create_task(
        master_cc.run_message(
            cfg,
            SessionMetadata(id=session_id, name="t"),
            "extra",
            callbacks,
            skip_user_event=True,
        ))

  async def fake_run_cc(item: master_cc._WorkItem) -> tuple:
    entries.append(thinking_state.busy_since(session_id))
    if inject_at == "run_cc":
      await _inject_once()
      await asyncio.sleep(0)
    return ("cc-1", 0, None, {})

  async def persist_hook(sid: str, event: dict) -> None:
    if inject_at == "master_done_persist" and event.get("type") == ET.MASTER_DONE:
      await _inject_once()
      await asyncio.sleep(0)
    await real_persist(sid, event)

  async def broadcast_hook(channel: str, event: dict) -> None:
    if (inject_at == "idle_broadcast" and event.get("type") == ET.RUNNING_CHANGED and
        event.get("thinking_since") is None):
      await _inject_once()
      await asyncio.sleep(0)

  async def probe_hook(sid: str) -> bool:
    if inject_at == "worker_probe":
      await _inject_once()
      await asyncio.sleep(0)
    return False

  workers_mock = MagicMock()
  workers_mock._has_running_tasks = probe_hook
  callbacks = SessionCallbacks(
      persist_and_broadcast=persist_hook,
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      persist_cc_session_id=AsyncMock(side_effect=lambda sid, ccid: ccid),
      has_completed_round=AsyncMock(return_value=False),
      persist_master_run=AsyncMock(),
  )

  monkeypatch.setattr(master_cc_run, "_run_cc", fake_run_cc)
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", broadcast_hook)
  monkeypatch.setattr("src.core.sessions.SessionManager", lambda *a, **k: workers_mock)

  _reset_master_state(session_id)
  try:
    task1 = asyncio.create_task(
        master_cc.run_message(cfg, SessionMetadata(id=session_id, name="t"), "first", callbacks, skip_user_event=True))
    assert await asyncio.wait_for(task1, timeout=5) == "cc-1"

    # For injections fired during the consumer's teardown awaits, the work item
    # only appears once that await runs — let the consumer reach it.
    for _ in range(1000):
      if injected_task is not None:
        break
      await asyncio.sleep(0)
    assert injected_task is not None, f"injection did not fire at {inject_at}"
    assert await asyncio.wait_for(injected_task, timeout=5) == "cc-1"

    # Let the (possibly second) consumer task finish its teardown.
    remaining = master_cc_state._session_consumers.get(session_id)
    if remaining is not None:
      await asyncio.wait_for(remaining, timeout=5)

    assert len(entries) == 2
    assert all(start is not None for start in entries), "every _run_cc entry must observe busy_since set"
    assert thinking_state.busy_since(session_id) is None
  finally:
    _reset_master_state(session_id)


def test_thinking_since_listed_as_transient() -> None:
  """T2a: de-persistence is structural, not incidental file content."""
  assert "thinking_since" in sessions_module._TRANSIENT_METADATA_FIELDS


@pytest.mark.asyncio
async def test_thinking_since_does_not_survive_save_reload_round_trip(tmp_path: Path) -> None:
  """T2b: a save-then-reload round trip must not carry the derived field."""
  cfg = _make_consumer_cfg(tmp_path)
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="t2"))
  thinking_state.mark_busy(session.id)
  try:
    stamped = await mgr.get_session(session.id)
    assert stamped is not None
    assert stamped.thinking_since is not None

    # Saving the stamped object must not write the derived field.
    await mgr.save_metadata(stamped)
    on_disk = json.loads((cfg.sessions_dir / session.id / "metadata.json").read_text(encoding="utf-8"))
    assert "thinking_since" not in on_disk

    # Reloading with an empty busy map must not resurrect the earlier value.
    thinking_state.clear_busy(session.id)
    mgr._invalidate_cache(session.id)
    reloaded = await mgr.get_session(session.id)
    assert reloaded is not None
    assert reloaded.thinking_since is None
  finally:
    thinking_state.clear_busy(session.id)


@pytest.mark.asyncio
async def test_busy_cleared_when_run_cc_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """T3a: exception in _run_cc still converges — busy state clears at teardown."""
  session_id = "t3-raise"
  cfg = _make_consumer_cfg(tmp_path)
  monkeypatch.setattr(master_cc_queue, "get_tex_path", lambda: tmp_path / "missing.tex")

  async def exploding_run_cc(item: master_cc._WorkItem) -> tuple:
    raise RuntimeError("boom")

  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)
  callbacks = mock_session_callbacks()

  monkeypatch.setattr(master_cc_run, "_run_cc", exploding_run_cc)
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", AsyncMock())
  monkeypatch.setattr("src.core.sessions.SessionManager", lambda *a, **k: workers_mock)

  _reset_master_state(session_id)
  try:
    run_task = asyncio.create_task(
        master_cc.run_message(cfg, SessionMetadata(id=session_id, name="t"), "hi", callbacks, skip_user_event=True))
    with pytest.raises(RuntimeError, match="boom"):
      await asyncio.wait_for(run_task, timeout=5)
    consumer = master_cc_state._session_consumers.get(session_id)
    if consumer is not None:
      await asyncio.wait_for(consumer, timeout=5)
    assert thinking_state.busy_since(session_id) is None
  finally:
    _reset_master_state(session_id)


@pytest.mark.asyncio
async def test_consumer_cancelled_before_first_item_finally_does_not_raise() -> None:
  """T3b: a consumer that never got its first item must exit via the same finally.

  The loop variable `item` is unbound here; the teardown must not raise
  NameError trying to read cfg/auto_trigger off it.
  """
  session_id = "t3-cancel"
  _reset_master_state(session_id)
  master_cc_state._session_queues[session_id] = asyncio.Queue()
  task = asyncio.create_task(master_cc._session_consumer(session_id))
  master_cc_state._session_consumers[session_id] = task
  try:
    await asyncio.sleep(0)  # consumer is now suspended at queue.get()
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)
    # A NameError raised in the finally would replace the cancellation with a
    # regular exception; a clean cancellation means teardown skipped gracefully.
    assert task.cancelled()
  finally:
    _reset_master_state(session_id)


@pytest.mark.asyncio
async def test_status_endpoint_thinking_since_matches_busy_map(tmp_path: Path) -> None:
  """T4: push and pull agree — /api/sessions/status reports the busy map's value."""
  cfg = _make_consumer_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="t4"))
  started_at, _created = thinking_state.mark_busy(session.id)
  try:
    app = FastAPI()
    app.include_router(sessions_api.router, prefix="/api/sessions")
    app.dependency_overrides[get_session_manager] = lambda: session_mgr
    with TestClient(app) as client:
      response = client.get(f"/api/sessions/status?ids={session.id}")
    assert response.status_code == 200
    payload = response.json()[session.id]
    current = thinking_state.busy_since(session.id)
    assert current is not None
    assert payload["thinking_since"] == current.isoformat()
    assert payload["thinking_since"] == started_at.isoformat()
  finally:
    thinking_state.clear_busy(session.id)


@pytest.mark.asyncio
async def test_busy_session_value_returned_on_all_read_paths(tmp_path: Path) -> None:
  """T5a: for a busy session, every read path returns the live busy value."""
  cfg = _make_consumer_cfg(tmp_path)
  author = SessionManager(cfg)
  session = await author.create_session(CreateSessionRequest(name="t5-reads"))
  started_at, _created = thinking_state.mark_busy(session.id)
  try:
    reader = SessionManager(cfg)  # cold metadata cache -> disk-parse path
    disk_read = await reader.get_session(session.id)
    assert disk_read is not None
    assert disk_read.thinking_since == started_at
    cache_read = await reader.get_session(session.id)
    assert cache_read is not None
    assert cache_read.thinking_since == started_at

    listed = await reader.list_sessions()
    assert [s.thinking_since for s in listed if s.id == session.id] == [started_at]
    searched = await reader.search_sessions("t5-reads")
    assert [s.thinking_since for s in searched if s.id == session.id] == [started_at]
    active = reader.list_active_session_metas()
    assert [s.thinking_since for s in active if s.id == session.id] == [started_at]
  finally:
    thinking_state.clear_busy(session.id)


@pytest.mark.asyncio
async def test_every_metadata_return_path_overwrites_stamp(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """T5b: every public method handing out a SessionMetadata applies the stamp,
  including fresh-construction returns that bypass a read of a stored session."""
  cfg = _make_consumer_cfg(tmp_path)
  mgr = SessionManager(cfg)
  sentinel = datetime(2026, 1, 2, 3, 4, 5, tzinfo=timezone.utc)
  monkeypatch.setattr("src.core.sessions.busy_since", lambda _sid: sentinel)
  monkeypatch.setattr(BROADCAST_PATCH_TARGET, AsyncMock())

  created = await mgr.create_session(CreateSessionRequest(name="walk"))
  assert created.thinking_since == sentinel

  got = await mgr.get_session(created.id)
  assert got is not None
  assert got.thinking_since == sentinel

  listed = await mgr.list_sessions()
  assert [s.thinking_since for s in listed if s.id == created.id] == [sentinel]

  found = await mgr.search_sessions("walk")
  assert [s.thinking_since for s in found if s.id == created.id] == [sentinel]

  # One chat event so fork/elone have history to reference.
  events_path = mgr.get_chat_events_path(created.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(json.dumps({"type": "user", "content": "hi"}) + "\n", encoding="utf-8")

  renamed = await mgr.rename_session(created.id, "walk-2")
  assert renamed is not None
  assert renamed.thinking_since == sentinel

  read_back = await mgr.mark_read(created.id)
  assert read_back is not None
  assert read_back.thinking_since == sentinel

  starred = await mgr.star_session(created.id)
  assert starred is not None
  assert starred.thinking_since == sentinel
  unstarred = await mgr.unstar_session(created.id)
  assert unstarred is not None
  assert unstarred.thinking_since == sentinel

  grouped = await mgr.set_group(created.id, "g")
  assert grouped is not None
  assert grouped.thinking_since == sentinel

  forked = await mgr.fork_session(created.id)
  assert forked.thinking_since == sentinel

  sched = await mgr.create_session(CreateSessionRequest(name="Scheduled: w", scheduled_task="w-task"))
  assert sched.thinking_since == sentinel
  via_sched = await mgr.ensure_scheduled_session_backend("w-task", "fake")
  assert via_sched is not None
  assert via_sched.thinking_since == sentinel

  # elone archives the parent; run it after every other check on `created`.
  eloned = await mgr.elone_session(created.id, event_index=0)
  assert eloned.thinking_since == sentinel

  active = mgr.list_active_session_metas()
  assert all(m.thinking_since == sentinel for m in active)


@pytest.mark.asyncio
async def test_stamp_recovers_after_unrelated_save_resets_cached_object(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """T5c: an unrelated save_metadata rebuilds the cached object from
  transient-excluded JSON; a cache-hit read within the TTL must still return
  the live busy value (no 30s bounded None window)."""
  cfg = _make_consumer_cfg(tmp_path)
  mgr = SessionManager(cfg)
  monkeypatch.setattr(BROADCAST_PATCH_TARGET, AsyncMock())
  session = await mgr.create_session(CreateSessionRequest(name="t5-cache"))
  started_at, _created = thinking_state.mark_busy(session.id)
  try:
    await mgr.mark_unread(session.id)  # unrelated save fires every master round
    cached_meta, _cached_ts = mgr._metadata_cache[session.id]
    assert cached_meta.thinking_since is None, "post-save cache rebuild must drop the derived field"

    # Cache-hit read within the 30s TTL re-stamps on the way out.
    got = await mgr.get_session(session.id)
    assert got is not None
    assert got.thinking_since == started_at
  finally:
    thinking_state.clear_busy(session.id)


# ---------------------------------------------------------------------------
# Resume anchor single-owner persistence + pre-flight (regression tests)
# ---------------------------------------------------------------------------


class _NoopBackend:
  """Minimal backend double: yields no events, exits cleanly."""

  exit_code = 0
  stderr_text = ""
  terminated = False

  async def terminate(self) -> None:
    self.terminated = True

  async def run(self, prompt: str, cwd: str, env: dict):
    if False:
      yield {}  # keeps run() an async generator; the consumer's async-for would TypeError on a coroutine


@pytest.mark.asyncio
async def test_consumer_persists_cc_session_id_to_disk(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Anchor lands on disk: after one round through the consumer, a second,
  cold-cache SessionManager reads the cc_session_id the backend returned.

  This is the assertion both the 2026-03-30 and 2026-07-30 regressions were
  missing — every prior test asserted only in-memory objects.
  """
  cfg = _make_consumer_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="anchor-on-disk"))
  backend_returned_id = "cc-backend-session-42"

  async def fake_run_cc(item: master_cc._WorkItem) -> tuple:
    return (backend_returned_id, 0, None, {})

  monkeypatch.setattr(master_cc_run, "_run_cc", fake_run_cc)
  monkeypatch.setattr(master_cc_queue, "get_tex_path", lambda: tmp_path / "missing.tex")
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", AsyncMock())

  _reset_master_state(session.id)
  try:
    result = await master_cc.run_message(
        cfg, session, "hi", session_mgr.callbacks(), skip_user_event=True)
    assert result == backend_returned_id
    consumer = master_cc_state._session_consumers.get(session.id)
    if consumer and not consumer.done():
      await asyncio.wait_for(consumer, timeout=5)
  finally:
    _reset_master_state(session.id)

  # Cold-cache reader: a fresh SessionManager parses metadata.json from disk.
  cold_reader = SessionManager(cfg)
  cold_meta = await cold_reader.get_session(session.id)
  assert cold_meta is not None
  assert cold_meta.cc_session_id == backend_returned_id


@pytest.mark.asyncio
async def test_pre_flight_fires_anchor_missing_when_round_done_and_anchor_empty(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """Pre-flight: a resume-capable backend with an empty anchor but a completed
  round emits resume_context_dropped with reason='anchor_missing'."""
  cfg = _make_consumer_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="pre-flight"))
  # Seed a completed round so has_completed_round returns True; anchor stays empty.
  await session_mgr.save_chat_event(session.id, {"type": ET.MASTER_DONE, "exit_code": 0})
  session_mgr._chat_events.clear_cache(session.id)

  meta = await session_mgr.get_session(session.id)
  assert meta is not None
  assert meta.cc_session_id is None

  monkeypatch.setattr("src.agents.backends.registry.build_backend",
                      lambda *a, **k: _NoopBackend())
  patch_instructions_content(monkeypatch)
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", AsyncMock())

  item = make_work_item(
      cfg, meta, cfg.backend_options[0], user_content="next round", callbacks=session_mgr.callbacks())
  await master_cc._run_cc(item)

  events = session_mgr.load_chat_events_sync(session.id)
  dropped = [e for e in events if e.get("type") == ET.RESUME_CONTEXT_DROPPED]
  assert len(dropped) == 1
  assert dropped[0]["reason"] == "anchor_missing"


@pytest.mark.asyncio
async def test_persist_cc_session_id_same_id_does_not_advance_started_at(tmp_path: Path) -> None:
  """started_at records the backend session start: writing the same id on two
  consecutive rounds must not advance cc_session_started_at."""
  cfg = _make_consumer_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  session = await session_mgr.create_session(CreateSessionRequest(name="started-at-drift"))

  read1 = await session_mgr.persist_cc_session_id(session.id, "same-id")
  assert read1 == "same-id"
  meta1 = await session_mgr.get_session(session.id)
  assert meta1 is not None
  assert meta1.cc_session_id == "same-id"
  assert meta1.cc_session_started_at is not None
  started_at_1 = meta1.cc_session_started_at

  await asyncio.sleep(0.01)

  read2 = await session_mgr.persist_cc_session_id(session.id, "same-id")
  assert read2 == "same-id"
  meta2 = await session_mgr.get_session(session.id)
  assert meta2 is not None
  assert meta2.cc_session_started_at == started_at_1, (
      "writing the same id again must not advance cc_session_started_at")


@pytest.mark.asyncio
async def test_resume_reattach_uses_persisted_interval_start(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  """A re-attached turn keeps its persisted interval start on the live surface.

  enqueue_master_resume queues an item carrying resume_record; both the busy
  map and the RUNNING_CHANGED payload must stamp the interval start from that
  record's started_at (600s in the past here), not from now(). Deterministic:
  _resume_cc is parked on an asyncio.Event so nothing races the asserts.
  """
  started_at = datetime.now(timezone.utc) - timedelta(seconds=600)
  record = MasterRunRecord(
      pid=1234,
      pid_start="100",
      started_at=started_at,
      raw_log="<fake>",
  )
  session_id = "test-resume-since"
  release = asyncio.Event()

  async def fake_resume_cc(item: master_cc._WorkItem) -> tuple:
    await release.wait()
    return "cc-id", 0, None, {}

  broadcast = AsyncMock()
  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)

  monkeypatch.setattr(master_cc_run, "_resume_cc", fake_resume_cc)
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", broadcast)
  monkeypatch.setattr("src.core.sessions.SessionManager", lambda *a, **k: workers_mock)

  cfg = _make_consumer_cfg(tmp_path)
  meta = _make_meta(session_id)
  callbacks = mock_session_callbacks()

  _reset_master_state(session_id)
  try:
    future = await master_cc.enqueue_master_resume(
        cfg, meta, record, callbacks, is_alive=lambda: True)

    assert thinking_state.busy_since(session_id) == started_at, (
        "the live busy map must use the persisted interval start")

    payloads = [
        c.args[1] for c in broadcast.call_args_list
        if c.args[1].get("type") == ET.RUNNING_CHANGED
    ]
    assert payloads, "expected a RUNNING_CHANGED payload on re-attach"
    assert payloads[0]["thinking_since"] == record.started_at.isoformat(), (
        "the sidebar-indicator start must equal the persisted record start")

    release.set()
    consumer = master_cc_state._session_consumers.get(session_id)
    if consumer is not None:
      await asyncio.wait_for(consumer, timeout=5)
    await asyncio.wait_for(future, timeout=5)
  finally:
    _reset_master_state(session_id)


# ---------------------------------------------------------------------------
# Zero-output guard: a settled run with all-zero usage and no output fails loudly
# ---------------------------------------------------------------------------


class _EventsBackend:
  """Backend double that yields a fixed event stream, then exits with the given code."""

  terminated = False

  def __init__(self, events: list[dict], *, exit_code: int = 0, stderr_text: str = "") -> None:
    self.events = events
    self.exit_code = exit_code
    self.stderr_text = stderr_text

  async def terminate(self) -> None:
    self.terminated = True

  async def run(self, prompt: str, cwd: str, env: dict):
    for event in self.events:
      yield event


def _guard_events(cb: SessionCallbacks) -> tuple[list[dict], list[dict]]:
  """Split persisted/broadcast events into zero-output ERRORs and MASTER_DONEs."""
  errors = [
      c.args[1] for c in cb.persist_and_broadcast.await_args_list
      if c.args[1].get("type") == ET.ERROR
  ]
  dones = [
      c.args[1] for c in cb.persist_and_broadcast.await_args_list
      if c.args[1].get("type") == ET.MASTER_DONE
  ]
  return errors, dones


async def _run_stream_consumer(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    session_id: str,
    events: list[dict],
    *,
    exit_code: int = 0,
    stderr_text: str = "",
) -> SessionCallbacks:
  """Run a simulated event stream through the production consumer path."""
  cfg = _make_consumer_cfg(tmp_path)
  meta = _make_meta(session_id)
  cb = mock_session_callbacks()
  backend = _EventsBackend(events, exit_code=exit_code, stderr_text=stderr_text)

  monkeypatch.setattr("src.agents.backends.registry.build_backend", lambda *a, **k: backend)
  patch_instructions_content(monkeypatch)
  monkeypatch.setattr(master_cc_queue, "get_tex_path", lambda: tmp_path / "missing.tex")
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", AsyncMock())

  _reset_master_state(session_id)
  try:
    await master_cc.run_message(cfg, meta, "hi", cb, skip_user_event=True)
    consumer = master_cc_state._session_consumers.get(session_id)
    if consumer is not None:
      await asyncio.wait_for(consumer, timeout=5)
  finally:
    _reset_master_state(session_id)
  return cb


@pytest.mark.asyncio
async def test_zero_output_guard_fires_on_all_zero_result(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Positive: a result with all-zero usage and nothing else fails loudly."""
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "zero-pos", [make_result_event()])

  errors, dones = _guard_events(cb)
  assert len(errors) == 1
  assert "zero model output" in errors[0].get("message", "")
  assert "fresh session" in errors[0].get("message", "")
  assert "left unread" in errors[0].get("message", "")
  assert "LESSONS.md" in errors[0].get("message", "")
  assert dones and dones[0]["exit_code"] == 1, "MASTER_DONE must exit nonzero"
  cb.mark_unread.assert_awaited()


@pytest.mark.asyncio
async def test_zero_output_guard_skips_nonzero_usage(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Negative: a result with real output tokens must not fire the guard."""
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "zero-neg-usage", [make_result_event(output_tokens=5)])

  errors, dones = _guard_events(cb)
  assert errors == []
  assert dones and dones[0]["exit_code"] == 0, "clean run keeps its exit code"


@pytest.mark.asyncio
async def test_zero_output_guard_skips_assistant_text(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Negative: assistant text alongside an all-zero usage result must not fire."""
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "zero-neg-text",
      [make_text_event("hello"), make_result_event()])

  errors, dones = _guard_events(cb)
  assert errors == []
  assert dones and dones[0]["exit_code"] == 0


@pytest.mark.asyncio
async def test_zero_output_guard_skips_missing_result(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Negative: a stream that never settles with a result must not fire."""
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "zero-neg-noresult", [make_text_event("hello")])

  errors, dones = _guard_events(cb)
  assert errors == []
  assert dones and dones[0]["exit_code"] == 0


@pytest.mark.asyncio
async def test_zero_output_guard_covers_resume_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The guard fires on the resume (re-attach) outcome, not just fresh runs."""
  session_id = "zero-resume"
  started_at = datetime.now(timezone.utc) - timedelta(seconds=60)
  record = MasterRunRecord(pid=1234, pid_start="100", started_at=started_at, raw_log="<fake>")

  cfg = _make_consumer_cfg(tmp_path)
  meta = _make_meta(session_id)
  cb = mock_session_callbacks()

  async def fake_resume_cc(item: master_cc._WorkItem) -> tuple:
    return "cc-resumed-id", 0, None, {"zero_output": True}

  monkeypatch.setattr(master_cc_run, "_resume_cc", fake_resume_cc)
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", AsyncMock())
  monkeypatch.setattr("src.core.sessions.SessionManager", lambda *a, **k: MagicMock(
      _has_running_tasks=AsyncMock(return_value=False)))

  _reset_master_state(session_id)
  try:
    future = await master_cc.enqueue_master_resume(
        cfg, meta, record, cb, is_alive=lambda: True)
    await asyncio.wait_for(future, timeout=5)
    consumer = master_cc_state._session_consumers.get(session_id)
    if consumer is not None:
      await asyncio.wait_for(consumer, timeout=5)
  finally:
    _reset_master_state(session_id)

  errors, dones = _guard_events(cb)
  assert len(errors) == 1
  assert "cc-resumed" in errors[0].get("message", "")
  assert dones and dones[0]["exit_code"] == 1


@pytest.mark.asyncio
async def test_zero_output_guard_exempts_manual_compact(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Negative: a manual /compact turn settles zero-output by design — exempt.

  The compaction itself is the turn's output, so the guard must not fire: no
  zero-output ERROR, MASTER_DONE keeps the backend's own exit code, and the
  finish extras carry no zero_output flag.
  """
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "zero-neg-compact-manual",
      [
          {"type": "system", "subtype": "compact_boundary",
           "compact_metadata": {"trigger": "manual", "pre_tokens": 10, "post_tokens": 2}},
          make_result_event(),
      ])

  errors, dones = _guard_events(cb)
  assert errors == []
  assert dones and dones[0]["exit_code"] == 0, "exempt turn keeps the backend's own exit code"
  assert dones[0].get("zero_output") is not True, "master_done must carry no zero_output flag"
  cb.mark_unread.assert_awaited()


@pytest.mark.asyncio
async def test_zero_output_guard_fires_on_auto_compact(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Positive: an auto-compact boundary must be followed by model output — a
  silent turn after it is exactly the failure the guard exists for."""
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "zero-pos-compact-auto",
      [
          {"type": "system", "subtype": "compact_boundary",
           "compact_metadata": {"trigger": "auto", "pre_tokens": 10, "post_tokens": 2}},
          make_result_event(),
      ])

  errors, dones = _guard_events(cb)
  assert len(errors) == 1
  assert errors[0].get("message", "") == (
      "Master run produced zero model output (cc_session_id=fresh session): "
      "the turn settled with an all-zero usage result and no assistant text, "
      "thinking, tool use, or manual compaction. "
      "The triggering message was left unread. "
      "One known cause: opencode message-ID wraparound (LESSONS.md, 2026-08-14).")
  assert dones and dones[0]["exit_code"] == 1, "MASTER_DONE must exit nonzero"


@pytest.mark.asyncio
async def test_zero_output_guard_resume_exempts_manual_compact(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  """Re-attach: a manual boundary seen only in the raw log's pre-cursor region
  still exempts the turn (the restart scenario: the pre-restart process
  consumed the boundary line and crashed before consuming the result).

  Drives the REAL _resume_cc against a real raw NDJSON log and cursor file.
  The cursor sits past the boundary line, so the cursor-forward tail never
  sees the boundary: this test fails if the resume path reads only from the
  cursor.
  """
  session_id = "zero-resume-compact-manual"
  log_dir = tmp_path / "run"
  log_dir.mkdir(parents=True)
  raw_path = log_dir / runs.RAW_LOG_NAME
  boundary_line = json.dumps(
      {"type": "system", "subtype": "compact_boundary",
       "compact_metadata": {"trigger": "manual", "pre_tokens": 10, "post_tokens": 2}}
  ) + "\n"
  result_line = json.dumps(make_result_event()) + "\n"
  raw_path.write_text(boundary_line + result_line, encoding="utf-8")
  cursor_path = log_dir / runs.CURSOR_NAME
  runs.write_raw_cursor(cursor_path, len(boundary_line.encode("utf-8")))

  # pid=None: no liveness probe and no kill path; is_alive=False makes the
  # follower drain the file and stop instead of waiting out the post-result
  # timeout.
  record = MasterRunRecord(
      pid=None,
      pid_start=None,
      started_at=datetime.now(timezone.utc) - timedelta(seconds=60),
      raw_log=str(raw_path),
  )

  cfg = _make_consumer_cfg(tmp_path)
  meta = _make_meta(session_id)
  cb = mock_session_callbacks()

  monkeypatch.setattr(master_cc_run, "_build_fresh_translate", lambda *a, **k: (lambda event: [event]))
  monkeypatch.setattr(master_cc_queue.streaming_manager, "broadcast", AsyncMock())
  monkeypatch.setattr("src.core.sessions.SessionManager", lambda *a, **k: MagicMock(
      _has_running_tasks=AsyncMock(return_value=False)))

  _reset_master_state(session_id)
  try:
    future = await master_cc.enqueue_master_resume(
        cfg, meta, record, cb, is_alive=lambda: False)
    await asyncio.wait_for(future, timeout=5)
    consumer = master_cc_state._session_consumers.get(session_id)
    if consumer is not None:
      await asyncio.wait_for(consumer, timeout=5)
  finally:
    _reset_master_state(session_id)

  errors, dones = _guard_events(cb)
  assert errors == []
  assert dones and dones[0]["exit_code"] == 0, "exempt turn keeps the backend's own exit code"
  assert dones[0].get("zero_output") is not True, "master_done must carry no zero_output flag"
  cb.mark_unread.assert_awaited()


@pytest.mark.asyncio
async def test_zero_output_guard_passes_through_independent_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
  """Exemption is purely subtractive: a manual-boundary turn whose backend
  independently failed keeps exactly its own error and nonzero exit code — the
  guard adds nothing and rewrites nothing."""
  cb = await _run_stream_consumer(
      tmp_path, monkeypatch, "zero-passthrough",
      [
          {"type": "system", "subtype": "compact_boundary",
           "compact_metadata": {"trigger": "manual", "pre_tokens": 10, "post_tokens": 2}},
          make_result_event(),
      ],
      exit_code=2,
      stderr_text="boom",
  )

  errors, dones = _guard_events(cb)
  assert errors == [], "no zero-output ERROR may be synthesized on top of the backend's own"
  assist_errors = [
      c.args[1] for c in cb.persist_and_broadcast.await_args_list
      if c.args[1].get("type") == ET.ASSISTANT_ERROR
  ]
  assert [e.get("content") for e in assist_errors] == ["Agent error: boom"], (
      "exactly the backend's own error event passes through")
  assert dones and dones[0]["exit_code"] == 2, "the backend's nonzero exit code passes through"
