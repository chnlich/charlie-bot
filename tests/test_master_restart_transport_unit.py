"""Unit invariants for the master-side restart transport:

- ``unanswered_user_events`` picks exactly the real user events after the last
  MASTER_DONE, excluding per-event ids (never per-session).
- ``SessionManager.persist_master_run`` writes (and clears) the record so a
  restarted process reloading metadata.json sees the identical state.
- The consumer clears master_run only after MASTER_DONE is durable, so a
  crash mid-window replays (duplicate) rather than silently dropping.
- The cancel path's let-go rule: a covered transport whose record hit disk is
  detached (never terminated, no terminal state written); uncovered
  transports and unrecorded turns are terminated.
- ``cancel_master`` on an in-memory miss judges the on-disk record: a
  provably live detached turn gets the signal contract; an unprovable one
  gets no signal at all.
- Boot side: an unresolved backend option keeps a provably live record and
  clears a dead one, and the identity judgment completes before any door
  that can create a new turn.
"""

from __future__ import annotations

import asyncio
import json
import signal
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.agents import master_cc
from src.core import init as init_module
from src.core.config import CharlieBotConfig
from src.core.models import (
    BackendOption,
    CreateSessionRequest,
    MasterRunRecord,
    SessionCallbacks,
    SessionMetadata,
    utc_now,
)
from src.core.sessions import SessionManager


def _cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backend_options=[BackendOption(id="fake", label="Fake", type="cc-claude", model="fake-model")],
  )


# --- unanswered_user_events -------------------------------------------------


def _user(content: str, event_id: str) -> dict:
  return {"type": "user", "content": content, "id": event_id}


def test_unanswered_scan_picks_only_events_after_last_master_done() -> None:
  events = [
      _user("answered already", "e1"),
      {"type": "assistant", "id": "a1"},
      {"type": "master_done", "id": "d1"},
      _user("still open", "e2"),
      _user("queued behind it", "e3"),
  ]
  pending = init_module.unanswered_user_events(events, set())
  assert [e["id"] for e in pending] == ["e2", "e3"]


def test_unanswered_scan_excludes_the_recorded_turns_event_only() -> None:
  # Exclusion must be per-event: excluding e2 (the crashed turn's message)
  # must NOT also shield e3, which disappeared with the killed in-memory queue.
  events = [
      _user("running when killed", "e2"),
      _user("queued behind it", "e3"),
  ]
  pending = init_module.unanswered_user_events(events, {"e2"})
  assert [e["id"] for e in pending] == ["e3"]


def test_unanswered_scan_ignores_non_message_user_typed_events() -> None:
  events = [
      {"type": "master_done", "id": "d1"},
      {"type": "user", "content": {"tool_result": True}, "id": "e4"},  # tool bridge, not a message
      _user("real message", "e5"),
  ]
  pending = init_module.unanswered_user_events(events, set())
  assert [e["id"] for e in pending] == ["e5"]


# --- SessionManager.persist_master_run --------------------------------------


@pytest.mark.asyncio
async def test_master_run_record_round_trips_through_metadata_json(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  record = MasterRunRecord(
      pid=123,
      pid_start="456.78",
      started_at=utc_now(),
      raw_log="/x/y/agent.raw.ndjson",
      user_event_id="evt-1",
  )

  await session_mgr.persist_master_run(meta.id, record)

  # Fresh process semantics: parse the file directly, not the manager cache.
  raw = json.loads((cfg.sessions_dir / meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert raw["master_run"]["pid"] == 123
  assert raw["master_run"]["pid_start"] == "456.78"
  assert raw["master_run"]["raw_log"] == "/x/y/agent.raw.ndjson"
  assert raw["master_run"]["user_event_id"] == "evt-1"
  assert SessionMetadata.model_validate(raw).master_run == record

  await session_mgr.persist_master_run(meta.id, None)
  raw = json.loads((cfg.sessions_dir / meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert raw["master_run"] is None


@pytest.mark.asyncio
async def test_master_run_persist_does_not_clobber_unrelated_fields(tmp_path: Path) -> None:
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  await session_mgr.persist_cc_session_id(meta.id, "cc-anchor")

  record = MasterRunRecord(started_at=utc_now(), raw_log="/x/agent.raw.ndjson")
  await session_mgr.persist_master_run(meta.id, record)

  fresh = await session_mgr.get_session(meta.id)
  assert fresh is not None
  assert fresh.cc_session_id == "cc-anchor"
  assert fresh.master_run == record


# --- consumer clears master_run only after MASTER_DONE -----------------------


def _make_callbacks(persist_order: list[str]) -> SessionCallbacks:
  async def persist_and_broadcast(session_id: str, event: dict) -> None:
    if event.get("type") == "master_done":
      persist_order.append("master_done")

  async def persist_master_run(session_id: str, record) -> None:
    if record is None:
      persist_order.append("master_run_cleared")

  return SessionCallbacks(
      persist_and_broadcast=persist_and_broadcast,
      update_thinking_state=AsyncMock(),
      mark_unread=AsyncMock(),
      persist_cc_session_id=AsyncMock(side_effect=lambda sid, ccid: ccid),
      has_completed_round=AsyncMock(return_value=False),
      persist_master_run=persist_master_run,
  )


@pytest.mark.asyncio
async def test_consumer_clears_master_run_after_master_done(monkeypatch: pytest.MonkeyPatch) -> None:
  persist_order: list[str] = []
  callbacks = _make_callbacks(persist_order)
  session_meta = SessionMetadata(id="session-clear", name="t")

  async def fake_run_cc(item: master_cc._WorkItem):
    return "cc-1", 0, None, {}

  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)

  item = master_cc._WorkItem(
      cfg=_cfg(Path("/tmp/charliebot-unit")),
      session_meta=session_meta,
      user_content="hi",
      callbacks=callbacks,
      is_voice=False,
      auto_trigger=False,
      backend_option=None,
      extra_claude_flags=None,
      should_check_tex=False,
      future=asyncio.get_running_loop().create_future(),
  )
  master_cc._session_queues.pop(session_meta.id, None)
  master_cc._session_queues[session_meta.id] = asyncio.Queue()
  master_cc._session_queues[session_meta.id].put_nowait(item)
  try:
    with (
        patch.object(master_cc, "_run_cc", side_effect=fake_run_cc),
        patch.object(master_cc.streaming_manager, "broadcast", new=AsyncMock()),
        patch("src.core.sessions.SessionManager", return_value=workers_mock),
    ):
      await asyncio.wait_for(master_cc._session_consumer(session_meta.id), timeout=5)
  finally:
    master_cc._session_queues.pop(session_meta.id, None)
    master_cc._session_consumers.pop(session_meta.id, None)

  assert persist_order == ["master_done", "master_run_cleared"], (
      "the restart identity must outlive the result boundary: clearing it first "
      "would let a kill between the two writes silently drop the user message")


# --- cancel path: let-go vs terminate (graceful-restart transport) -----------


class _HungBackend:
  """A live backend that streams nothing; cancel-path tests cancel its owner task.

  ``terminate``/``detach`` are spies so each row of the let-go contract is
  asserted on the mechanism (which was called), never on a literal.
  """

  def __init__(self, *, fire_spawn: bool = True) -> None:
    self.pid_start = "424242.0"
    self.exit_code = 1
    self.stderr_text = ""
    self.terminated = False
    self.terminate = AsyncMock()
    self.detach = MagicMock()
    self.on_spawn = None
    self._fire_spawn = fire_spawn
    # Set after _on_spawn returned: the master_run record is on disk by then.
    self.spawned = asyncio.Event()
    # Set the moment the run loop starts, before any spawn callback.
    self.run_entered = asyncio.Event()

  async def run(self, prompt: str, cwd: str, env: dict):
    self.run_entered.set()
    if self._fire_spawn:
      await self.on_spawn(4242)
      self.spawned.set()
    await asyncio.Event().wait()
    yield  # pragma: no cover — cancellation always lands first


def _install_backend(monkeypatch: pytest.MonkeyPatch, backend: _HungBackend) -> None:
  def _build(*args, **kwargs):
    backend.on_spawn = kwargs["on_spawn"]
    return backend

  monkeypatch.setattr("src.agents.backends.registry.build_backend", _build)
  monkeypatch.setattr(master_cc, "_build_instructions_content", lambda session_meta, cfg: "instructions")


def _persisting_callbacks(session_mgr: SessionManager, *, mark_unread=None) -> SessionCallbacks:
  """Real persist_master_run against a tmp-home manager; everything else mocked."""
  return SessionCallbacks(
      persist_and_broadcast=AsyncMock(),
      update_thinking_state=AsyncMock(),
      mark_unread=mark_unread if mark_unread is not None else AsyncMock(),
      persist_cc_session_id=AsyncMock(side_effect=lambda sid, ccid: ccid),
      has_completed_round=AsyncMock(return_value=False),
      persist_master_run=session_mgr.persist_master_run,
  )


def _cancel_item(cfg: CharlieBotConfig, session_meta: SessionMetadata, callbacks: SessionCallbacks,
                 option: BackendOption, *, should_check_tex: bool = False) -> master_cc._WorkItem:
  return master_cc._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content="hi",
      callbacks=callbacks,
      is_voice=False,
      auto_trigger=False,
      backend_option=option,
      extra_claude_flags=None,
      should_check_tex=should_check_tex,
      future=asyncio.get_running_loop().create_future(),
      user_event_id="evt-1",
  )


async def _cancel_run(item: master_cc._WorkItem, ready: asyncio.Event) -> None:
  """Drive _run_cc until *ready*, then cancel — the event-loop shutdown trigger."""
  task = asyncio.create_task(master_cc._run_cc(item))
  await asyncio.wait_for(ready.wait(), timeout=5)
  task.cancel()
  with pytest.raises(asyncio.CancelledError):
    await task


@pytest.mark.asyncio
async def test_cancel_covered_turn_detaches_and_keeps_the_record(tmp_path: Path,
                                                                  monkeypatch: pytest.MonkeyPatch) -> None:
  """Covered transport + persisted record: the turn is handed to the next boot."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  backend = _HungBackend()
  _install_backend(monkeypatch, backend)

  await _cancel_run(
      _cancel_item(cfg, meta, _persisting_callbacks(session_mgr), cfg.backend_options[0]),
      backend.spawned)

  backend.detach.assert_called_once_with()
  backend.terminate.assert_not_awaited()
  # Fresh-process semantics: the record the next boot reconciles is on disk.
  raw = json.loads((cfg.sessions_dir / meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert raw["master_run"] is not None
  assert raw["master_run"]["pid"] == 4242
  assert raw["master_run"]["pid_start"] == "424242.0"
  assert raw["master_run"]["user_event_id"] == "evt-1"


@pytest.mark.asyncio
async def test_cancel_uncovered_transport_terminates(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An uncovered transport dies with its transport process even mid-turn."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  backend = _HungBackend()
  _install_backend(monkeypatch, backend)
  option = BackendOption(id="agy", label="Agy", type="antigravity", model=None)

  await _cancel_run(_cancel_item(cfg, meta, _persisting_callbacks(session_mgr), option), backend.spawned)

  backend.terminate.assert_awaited_once_with()
  backend.detach.assert_not_called()


@pytest.mark.asyncio
async def test_cancel_before_record_persisted_terminates(tmp_path: Path,
                                                         monkeypatch: pytest.MonkeyPatch) -> None:
  """A turn whose record never hit disk can never be found by a boot: terminate."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  backend = _HungBackend(fire_spawn=False)
  _install_backend(monkeypatch, backend)

  await _cancel_run(
      _cancel_item(cfg, meta, _persisting_callbacks(session_mgr), cfg.backend_options[0]),
      backend.run_entered)

  backend.terminate.assert_awaited_once_with()
  backend.detach.assert_not_called()
  raw = json.loads((cfg.sessions_dir / meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert raw["master_run"] is None


@pytest.mark.asyncio
async def test_let_go_writes_no_terminal_state(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The let-go path lies about nothing: no unread marker, no tex check, no
  finished log — the next boot's reconcile owns the outcome. The backend is
  still dropped from _active_procs."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="t"))
  backend = _HungBackend()
  _install_backend(monkeypatch, backend)
  mark_unread = AsyncMock()
  check_tex = MagicMock(return_value=None)
  clear_snapshot = MagicMock()
  monkeypatch.setattr(master_cc, "check_tex_changed", check_tex)
  monkeypatch.setattr(master_cc, "clear_snapshot", clear_snapshot)

  # should_check_tex=True so a non-let-go cancel WOULD run the tex row.
  await _cancel_run(
      _cancel_item(
          cfg, meta, _persisting_callbacks(session_mgr, mark_unread=mark_unread),
          cfg.backend_options[0], should_check_tex=True),
      backend.spawned)

  backend.detach.assert_called_once_with()
  mark_unread.assert_not_awaited()
  check_tex.assert_not_called()
  clear_snapshot.assert_not_called()
  assert cfg.sessions_dir.joinpath(meta.id).exists()  # sanity: the session dir is the tmp home's
  assert meta.id not in master_cc._active_procs


# --- cancel_master falls back to the on-disk master_run record ---------------


@pytest.mark.asyncio
async def test_cancel_master_kills_a_live_detached_record(monkeypatch: pytest.MonkeyPatch) -> None:
  """In-memory miss + provably live record: the turn kept running detached
  across a graceful restart, so the recorded pid group gets the SIGTERM grace
  -> SIGKILL contract, the record is cleared, and the endpoint gets its True."""
  session_id = "session-detached"
  record = MasterRunRecord(
      pid=4242,
      pid_start="1.0",
      started_at=utc_now(),
      raw_log="/x/agent.raw.ndjson",
      user_event_id="evt-1",
  )
  meta = SessionMetadata(id=session_id, name="t", master_run=record)
  session_mgr = AsyncMock()
  kill = MagicMock()
  # Alive at the judgment, gone after the SIGTERM: the SIGKILL never goes out.
  alive = iter([True, False, False])
  monkeypatch.setattr(master_cc.runs, "is_run_alive", lambda *args: next(alive))
  monkeypatch.setattr(master_cc, "kill_process_group", kill)
  master_cc._active_procs.pop(session_id, None)

  result = await master_cc.cancel_master(session_id, meta=meta, session_mgr=session_mgr)

  assert result is True
  assert kill.call_count == 1
  assert kill.call_args_list[0].args == (4242, signal.SIGTERM)
  session_mgr.persist_master_run.assert_awaited_once_with(session_id, None)


@pytest.mark.asyncio
async def test_cancel_master_never_signals_an_unprovable_record(monkeypatch: pytest.MonkeyPatch) -> None:
  """The opencode-shaped record: pid_start is never set by that backend, so
  liveness is unprovable and an irreversible signal must NOT go out — the
  common restart shape, not an edge case. is_run_alive stays real here: the
  pin is the whole chain from record shape to no-signal."""
  session_id = "session-unprovable"
  record = MasterRunRecord(
      pid=4242,
      pid_start=None,
      started_at=utc_now(),
      raw_log="/x/agent.raw.ndjson",
      user_event_id="evt-1",
  )
  meta = SessionMetadata(id=session_id, name="t", master_run=record)
  session_mgr = AsyncMock()
  kill = MagicMock()
  monkeypatch.setattr(master_cc, "kill_process_group", kill)

  result = await master_cc.cancel_master(session_id, meta=meta, session_mgr=session_mgr)

  assert result is False
  kill.assert_not_called()
  session_mgr.persist_master_run.assert_not_awaited()


# --- boot side: the unresolved-option identity rows ---------------------------


@pytest.mark.asyncio
async def test_identity_unresolved_option_keeps_live_record_clears_dead_one(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A session pinning a backend id config no longer defines cannot be judged
  through the outcome table, so the record judges: provably alive keeps the
  record and excludes exactly its user event from replay; unprovable clears
  the record and leaves the message to the replay pass."""
  cfg = _cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  live_meta = await session_mgr.create_session(CreateSessionRequest(name="live"), backend="gone")
  dead_meta = await session_mgr.create_session(CreateSessionRequest(name="dead"), backend="gone")
  live_user = {"type": "user", "content": "live msg"}
  dead_user = {"type": "user", "content": "dead msg"}
  await session_mgr.save_chat_event(live_meta.id, live_user)
  await session_mgr.save_chat_event(dead_meta.id, dead_user)
  started_at = datetime.now(timezone.utc) - timedelta(seconds=60)
  await session_mgr.persist_master_run(
      live_meta.id,
      MasterRunRecord(
          pid=1111,
          pid_start="9.0",
          started_at=started_at,
          raw_log=str(tmp_path / "a" / "agent.raw.ndjson"),
          user_event_id=live_user["id"],
      ))
  await session_mgr.persist_master_run(
      dead_meta.id,
      MasterRunRecord(
          pid=2222,
          pid_start=None,
          started_at=started_at,
          raw_log=str(tmp_path / "b" / "agent.raw.ndjson"),
          user_event_id=dead_user["id"],
      ))
  # Liveness is judged on the record triple; pin it on the live one only.
  monkeypatch.setattr(
      init_module.runs, "is_run_alive",
      lambda pid, pid_start, started_at, host_boot: (pid, pid_start) == (1111, "9.0"))

  excluded = await init_module.reconcile_master_identity(cfg, session_mgr, datetime.now(timezone.utc))

  assert excluded == {live_meta.id: {live_user["id"]}}, (
      "the live turn's message is its owner's alone; the dead turn's message stays replayable")
  live_raw = json.loads((cfg.sessions_dir / live_meta.id / "metadata.json").read_text(encoding="utf-8"))
  dead_raw = json.loads((cfg.sessions_dir / dead_meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert live_raw["master_run"]["pid"] == 1111  # kept: still judged next restart
  assert dead_raw["master_run"] is None         # cleared: replay answers its message
  live_events = [json.loads(line) for line in
                 (cfg.sessions_dir / live_meta.id / "data" / "chat_events.jsonl")
                 .read_text(encoding="utf-8").splitlines()]
  assert any(
      "treated as still alive (backend option 'gone' unresolved)" in json.dumps(e)
      for e in live_events)


# --- boot ordering: identity judgment before any new-turn door ----------------


@pytest.mark.asyncio
async def test_identity_judgment_runs_before_any_new_turn_door(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """master_run is a single slot per session that a spawning turn overwrites
  blindly, so the identity judgment must complete before the crash-recovery
  task, scheduler.start(), and trigger recovery. Asserted on recorded call
  order, never on timing."""
  import server
  from src.api import deps
  from src.core import config as core_config
  from src.core.scheduler import Scheduler
  from src.core.triggers import TriggerManager

  home = tmp_path / "home"
  monkeypatch.setenv("CHARLIEBOT_HOME", str(home))
  monkeypatch.setattr(core_config, "_config", None)
  monkeypatch.setattr(core_config, "_config_mtime", 0.0)
  monkeypatch.setattr(deps, "_session_manager", None)
  monkeypatch.setattr(deps, "_thread_manager", None)
  monkeypatch.setattr(deps, "_trigger_manager", None)

  calls: list[str] = []

  async def fake_identity(cfg, session_mgr, boot_time):
    calls.append("identity")
    return {}

  async def fake_recovery(cfg, boot_time, identity=None):
    calls.append("crash_recovery")

  async def fake_scheduler_start(self):
    calls.append("scheduler.start")

  async def fake_recover_pending(self):
    calls.append("trigger.recover_pending")

  monkeypatch.setattr(server, "reconcile_master_identity", fake_identity)
  monkeypatch.setattr(server, "_run_crash_recovery", fake_recovery)
  monkeypatch.setattr(Scheduler, "start", fake_scheduler_start)
  monkeypatch.setattr(Scheduler, "stop", AsyncMock())
  monkeypatch.setattr(TriggerManager, "recover_pending", fake_recover_pending)
  # Background starters are not the doors under test; keep them off the net.
  monkeypatch.setattr(server.transcriber, "start_model_provisioning", lambda cfg: None)
  monkeypatch.setattr(server.ext_usage, "start_poller", AsyncMock())
  monkeypatch.setattr(server.ext_usage, "stop_poller", AsyncMock())

  from fastapi import FastAPI
  async with server.lifespan(FastAPI()):
    await asyncio.sleep(0.2)  # let the background recovery task record its call

  for door in ("crash_recovery", "scheduler.start", "trigger.recover_pending"):
    assert door in calls, f"{door} never ran; the ordering assertion would be vacuous"
    assert calls.index("identity") < calls.index(door), f"identity judged after {door}: {calls}"


# --- queued_user_event_ids ----------------------------------------------------


@pytest.mark.asyncio
async def test_queued_user_event_ids_covers_running_and_queued_items() -> None:
  gate = asyncio.Event()

  async def blocked_run_cc(item: master_cc._WorkItem):
    await gate.wait()
    return None, 0, None, {}

  callbacks = _make_callbacks([])
  cfg = _cfg(Path("/tmp/charliebot-unit"))
  session_meta = SessionMetadata(id="session-queued", name="t")

  def item(event_id: str) -> master_cc._WorkItem:
    return master_cc._WorkItem(
        cfg=cfg,
        session_meta=session_meta,
        user_content="x",
        callbacks=callbacks,
        is_voice=False,
        auto_trigger=False,
        backend_option=None,
        extra_claude_flags=None,
        should_check_tex=False,
        future=asyncio.get_running_loop().create_future(),
        user_event_id=event_id,
    )

  running = item("evt-running")
  queued = item("evt-queued")
  workers_mock = MagicMock()
  workers_mock._has_running_tasks = AsyncMock(return_value=False)
  try:
    with (
        patch.object(master_cc, "_run_cc", side_effect=blocked_run_cc),
        patch.object(master_cc.streaming_manager, "broadcast", new=AsyncMock()),
        patch("src.core.sessions.SessionManager", return_value=workers_mock),
    ):
      master_cc._enqueue_work_item(session_meta.id, running)
      master_cc._enqueue_work_item(session_meta.id, queued)
      # Let the consumer pick up the first item.
      deadline = time.monotonic() + 2.0
      while session_meta.id not in master_cc._current_items and time.monotonic() < deadline:
        await asyncio.sleep(0.01)
      ids = master_cc.queued_user_event_ids(session_meta.id)
      assert ids == {"evt-running", "evt-queued"}
  finally:
    gate.set()
    await asyncio.gather(*(i.future for i in (running, queued)), return_exceptions=True)
    master_cc._session_queues.pop(session_meta.id, None)
    master_cc._session_consumers.pop(session_meta.id, None)
