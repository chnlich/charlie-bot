"""Tests for the elone succession pointer and successor-chain resolution."""

from __future__ import annotations

import itertools
import json
from datetime import UTC, datetime, timedelta
from pathlib import Path
from unittest.mock import AsyncMock, patch
from zoneinfo import ZoneInfo

import pytest
import yaml
from conftest import (
  BROADCAST_PATCH_TARGET,
  TRIGGER_MASTER_PATCH_TARGET,
  TRIGGERS_GET_CONFIG_PATCH_TARGET,
  build_sessions_cfg,
  build_two_backend_cfg,
  make_home_session,
)
from conftest import append_events as _append_events
from conftest import make_parent as _make_parent
from conftest import make_sessions_client as _build_client
from conftest import session_dir_names as _session_dir_names

from src.core.config import (
  CharlieBotConfig,
  ScheduledTaskConfig,
  _load_cron_file,
)
from src.core.models import (
  PROJECT_ROLE,
  CreateSessionRequest,
  SessionMetadata,
  SessionStatus,
  TriggerStatus,
)
from src.core.scheduled_sessions import ScheduledSessionStore
from src.core.scheduler import Scheduler
from src.core.sessions import (
  ScheduledSessionBusyError,
  SessionManager,
  SuccessionRefused,
)
from src.core.thinking_state import clear_busy, mark_busy
from src.core.triggers import TriggerManager

# Cron that fires every minute: a last_scheduled_run inside the current minute
# window suppresses the tick, while a missing one makes the never-run branch
# reach back over that same window and fire immediately.
_CADENCE_CRON = "* * * * *"


def _seed_scheduled_task(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    task_name: str = "nightly",
    cron: str = "0 2 * * *",
    backend: str = "claude-opus-4.6",
) -> Path:
  """Seed a prompt_file-backed host cron file and point the core backend-write helper at it.

  Shaped like a production host file (path to prompt source under
  ``prompt_file``), and resolvable through the production loader, so a
  scheduler tick can rebuild the task config from what the write-back left on
  disk.
  """
  prompt = tmp_path / "prompts" / f"{task_name}.md"
  prompt.parent.mkdir(parents=True, exist_ok=True)
  prompt.write_text("run nightly", encoding="utf-8")
  cron_d = tmp_path / "cron.d"
  cron_d.mkdir(parents=True, exist_ok=True)
  path = cron_d / f"{task_name}.yaml"
  path.write_text(
      yaml.safe_dump({
          "cron": cron,
          "prompt_file": str(prompt),
          "timezone": "America/Los_Angeles",
          "backend": backend,
      }),
      encoding="utf-8")
  monkeypatch.setattr(
      "src.core.scheduled_sessions.cron_path",
      lambda name: cron_d / f"{name}.yaml")
  return path


async def _make_scheduled_parent(
    mgr: SessionManager,
    *,
    task: str = "nightly",
    name: str = "Scheduled: nightly",
    backend: str = "claude-opus-4.6",
    role: str | None = None,
    group: str | None = None,
    events: int = 3,
) -> SessionMetadata:
  """Create the active scheduled session for *task*, with *events* chat events."""
  parent = await mgr.create_session(
      CreateSessionRequest(name=name, scheduled_task=task, role=role), backend=backend)
  if group is not None:
    parent.group = group
    await mgr.save_metadata(parent)
  _append_events(
      mgr.get_chat_events_path(parent.id),
      [{"type": "user", "content": f"e{i}"} for i in range(events)],
  )
  return parent


@pytest.mark.asyncio
async def test_elone_writes_successor_pointer_and_archives_thumbs_down_parent(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)

  child = await mgr.elone_session(parent_id, event_index=0)

  fresh_parent = await mgr.read_metadata_fresh(parent_id)
  assert fresh_parent is not None
  assert fresh_parent.successor_session_id == child.id
  assert fresh_parent.status == SessionStatus.ARCHIVED
  assert fresh_parent.rating == "thumbs_down"


@pytest.mark.asyncio
async def test_second_elone_of_ordinary_parent_overwrites_successor_and_leaves_first_child_untouched(
    tmp_path: Path,
) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)

  first_child = await mgr.elone_session(parent_id, event_index=0)
  second_child = await mgr.elone_session(parent_id, event_index=0)

  assert first_child.id != second_child.id

  # Both children carry the parent reference handoff.
  first_ref = mgr.parent_reference_path(first_child.id)
  second_ref = mgr.parent_reference_path(second_child.id)
  assert first_ref.exists()
  assert second_ref.exists()

  # The parent is archived with thumbs_down and the pointer names the newest child.
  fresh_parent = await mgr.read_metadata_fresh(parent_id)
  assert fresh_parent is not None
  assert fresh_parent.status == SessionStatus.ARCHIVED
  assert fresh_parent.rating == "thumbs_down"
  assert fresh_parent.successor_session_id == second_child.id

  # The first child's metadata is untouched by the second elone.
  first_meta = await mgr.read_metadata_fresh(first_child.id)
  assert first_meta is not None
  assert first_meta.status == SessionStatus.ACTIVE
  assert first_meta.successor_session_id is None

  # Chain resolution and delivery both land at the newest (second) child.
  resolved = await mgr.resolve_successor_chain(parent_id)
  assert resolved is not None
  assert resolved.id == second_child.id

  event = {"type": "user", "content": "delivered to newest"}
  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    delivered = await mgr.deliver_to_successor(parent_id, event)

  assert delivered == second_child.id
  assert event.get("origin_session_id") == parent_id
  assert any(ev.get("content") == "delivered to newest" for ev in mgr.load_chat_events_sync(second_child.id))


@pytest.mark.asyncio
async def test_elone_of_scheduler_owned_session_succeeds_with_full_inheritance(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  yaml_path = _seed_scheduled_task(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr, role=PROJECT_ROLE, group="proj-a")
  parent.last_scheduled_run = "2026-08-20T02:00:00-07:00"
  parent.last_scheduled_cron = "0 2 * * *"
  parent.last_run_status = "success"
  await mgr.save_metadata(parent)
  yaml_before = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))

  child = await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")

  # The successor is the task's next generation: scheduling identity, group,
  # and bookkeeping ride along on the requested backend, and the name stays
  # the parent's verbatim (an ordinary elone would have prefixed it).
  assert child.parent_session_id == parent.id
  assert child.backend == "codex-o3"
  assert child.scheduled_task == parent.scheduled_task
  assert child.role == parent.role
  assert child.group == parent.group
  assert child.name == parent.name
  assert child.last_scheduled_run == parent.last_scheduled_run
  assert child.last_scheduled_cron == parent.last_scheduled_cron
  assert child.last_run_status == parent.last_run_status

  fresh_parent = await mgr.read_metadata_fresh(parent.id)
  assert fresh_parent is not None
  assert fresh_parent.status == SessionStatus.ARCHIVED
  assert fresh_parent.rating == "thumbs_down"
  assert fresh_parent.successor_session_id == child.id
  # The archived parent keeps its scheduled_task field; the alignment scan
  # only considers active sessions, so it stays inert.
  assert fresh_parent.scheduled_task == parent.scheduled_task

  # The write-back touched only the backend key of the task yaml.
  yaml_after = yaml.safe_load(yaml_path.read_text(encoding="utf-8"))
  assert yaml_after == {**yaml_before, "backend": "codex-o3"}


@pytest.mark.asyncio
async def test_second_elone_of_scheduler_owned_parent_refuses_and_mutates_nothing(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  _seed_scheduled_task(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr)

  first_child = await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")
  before = _session_dir_names(cfg)

  with pytest.raises(SuccessionRefused):
    await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")

  # No new session directory appears.
  assert _session_dir_names(cfg) == before

  # The parent's metadata is unchanged: successor still the first child,
  # archived, thumbs_down.
  fresh_parent = await mgr.read_metadata_fresh(parent.id)
  assert fresh_parent is not None
  assert fresh_parent.successor_session_id == first_child.id
  assert fresh_parent.status == SessionStatus.ARCHIVED
  assert fresh_parent.rating == "thumbs_down"


@pytest.mark.asyncio
async def test_resolve_successor_chain_returns_session_itself_when_no_successor(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)

  resolved = await mgr.resolve_successor_chain(parent_id)
  assert resolved is not None
  assert resolved.id == parent_id


@pytest.mark.asyncio
async def test_resolve_successor_chain_walks_across_three_generations(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  gen0 = await _make_parent(mgr, name="G0")

  gen1 = await mgr.elone_session(gen0, event_index=0)
  gen2 = await mgr.elone_session(gen1.id, event_index=0)
  gen3 = await mgr.elone_session(gen2.id, event_index=0)

  resolved = await mgr.resolve_successor_chain(gen0)
  assert resolved is not None
  assert resolved.id == gen3.id


@pytest.mark.asyncio
async def test_resolve_successor_chain_allows_exactly_100_hops(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  sessions = [
      await mgr.create_session(CreateSessionRequest(name=f"Generation {i}"), backend="claude-opus-4.6")
      for i in range(101)
  ]

  for current, successor in itertools.pairwise(sessions):
    meta = await mgr.read_metadata_fresh(current.id)
    assert meta is not None
    meta.successor_session_id = successor.id
    await mgr.save_metadata(meta)

  resolved = await mgr.resolve_successor_chain(sessions[0].id)
  assert resolved is not None
  assert resolved.id == sessions[-1].id


@pytest.mark.asyncio
async def test_resolve_successor_chain_stops_at_last_existing_when_mid_chain_deleted(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  gen0 = await _make_parent(mgr, name="G0")
  gen1 = await mgr.elone_session(gen0, event_index=0)
  await mgr.elone_session(gen1.id, event_index=0)

  await mgr.delete_session_permanently(gen1.id)

  # gen0 -> gen1 (deleted). The walk stops at gen0, the last existing session.
  resolved = await mgr.resolve_successor_chain(gen0)
  assert resolved is not None
  assert resolved.id == gen0


@pytest.mark.asyncio
async def test_elone_after_successor_permanently_deleted_repairs_pointer(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr, name="G0")

  first_child = await mgr.elone_session(parent_id, event_index=0)
  await mgr.delete_session_permanently(first_child.id)

  # The parent's successor was deleted, so it re-elones freely; the call
  # succeeds and the pointer is repaired to the new child.
  new_child = await mgr.elone_session(parent_id, event_index=0)

  fresh_parent = await mgr.read_metadata_fresh(parent_id)
  assert fresh_parent is not None
  assert fresh_parent.successor_session_id == new_child.id

  resolved = await mgr.resolve_successor_chain(parent_id)
  assert resolved is not None
  assert resolved.id == new_child.id


@pytest.mark.asyncio
async def test_resolve_successor_chain_returns_none_for_missing_session(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  assert await mgr.resolve_successor_chain("does-not-exist") is None


@pytest.mark.asyncio
async def test_resolve_successor_chain_raises_runtime_error_on_cycle(tmp_path: Path) -> None:
  _cfg, mgr, a = await make_home_session(tmp_path, name="A", backend="claude-opus-4.6")
  b = await mgr.create_session(CreateSessionRequest(name="B"), backend="claude-opus-4.6")
  a_meta = await mgr.read_metadata_fresh(a.id)
  b_meta = await mgr.read_metadata_fresh(b.id)
  assert a_meta is not None and b_meta is not None
  a_meta.successor_session_id = b.id
  b_meta.successor_session_id = a.id
  await mgr.save_metadata(a_meta)
  await mgr.save_metadata(b_meta)

  with pytest.raises(RuntimeError, match="100 hops"):
    await mgr.resolve_successor_chain(a.id)


@pytest.mark.asyncio
async def test_read_metadata_fresh_sees_successor_written_after_get_session_cached(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)

  # Prime the TTL cache so get_session serves the pre-successor value.
  cached = await mgr.get_session(parent_id)
  assert cached is not None
  assert cached.successor_session_id is None

  # A concurrent elone (another manager/process) writes the successor to disk
  # directly, bypassing this manager's cache.
  other = SessionManager(cfg)
  child = await other.elone_session(parent_id, event_index=0)

  # The normal read remains stale, while the fresh read reflects the disk write.
  stale = await mgr.get_session(parent_id)
  assert stale is not None
  assert stale.successor_session_id is None
  fresh = await mgr.read_metadata_fresh(parent_id)
  assert fresh is not None
  assert fresh.successor_session_id == child.id


@pytest.mark.asyncio
async def test_api_succession_refused_maps_to_409_and_bad_event_index_to_400(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_sessions_cfg(tmp_path)
  mgr = SessionManager(cfg)
  client = _build_client(cfg, mgr)

  parent_id = await _make_parent(mgr)

  # Both elones of an ordinary parent succeed (200).
  with client:
    resp = client.post(f"/api/sessions/{parent_id}/elone", json={"event_index": 0})
    assert resp.status_code == 200
    resp = client.post(f"/api/sessions/{parent_id}/elone", json={"event_index": 0})
    assert resp.status_code == 200

  # A scheduler-owned parent refuses its second elone -> 409 (quiet parent).
  scheduled_mgr = SessionManager(cfg)
  scheduled_client = _build_client(cfg, scheduled_mgr)
  _seed_scheduled_task(tmp_path, monkeypatch)
  scheduled_parent = await _make_scheduled_parent(scheduled_mgr)
  with scheduled_client:
    resp = scheduled_client.post(
        f"/api/sessions/{scheduled_parent.id}/elone", json={"event_index": 0})
    assert resp.status_code == 200
    resp = scheduled_client.post(
        f"/api/sessions/{scheduled_parent.id}/elone", json={"event_index": 0})
    assert resp.status_code == 409
    assert "already has a successor" in resp.json()["detail"]

  # Out-of-range event_index on a fresh parent: plain ValueError -> 400.
  other_parent = await _make_parent(mgr, name="Other")
  with client:
    resp = client.post(f"/api/sessions/{other_parent}/elone", json={"event_index": 99})
    assert resp.status_code == 400
    assert "out of range" in resp.json()["detail"]


@pytest.mark.asyncio
async def test_deliver_to_successor_writes_into_chain_end_and_stamps_origin(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  gen0 = await _make_parent(mgr, name="G0")
  gen1 = await mgr.elone_session(gen0, event_index=0)
  gen2 = await mgr.elone_session(gen1.id, event_index=0)

  event = {"type": "user", "content": "delivered"}
  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    delivered = await mgr.deliver_to_successor(gen0, event)

  assert delivered == gen2.id
  assert event.get("origin_session_id") == gen0
  assert any(ev.get("content") == "delivered" for ev in mgr.load_chat_events_sync(gen2.id))


@pytest.mark.asyncio
async def test_deliver_to_successor_leaves_origin_absent_for_no_successor(tmp_path: Path) -> None:
  mgr = SessionManager(CharlieBotConfig(charliebot_home=tmp_path / "home"))
  gen0 = await _make_parent(mgr)

  event = {"type": "user", "content": "no redirect"}
  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    delivered = await mgr.deliver_to_successor(gen0, event)

  assert delivered == gen0
  assert "origin_session_id" not in event
  assert any(ev.get("content") == "no redirect" for ev in mgr.load_chat_events_sync(gen0))


@pytest.mark.asyncio
async def test_deliver_to_successor_returns_none_and_writes_nothing_when_chain_end_dir_removed(
    tmp_path: Path,
) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Gone", backend="claude-opus-4.6")

  # Remove the whole session directory, including metadata.json, exactly as a
  # permanent delete does. append_ndjson would recreate the dir — assert it does not.
  await mgr.delete_session_permanently(session.id)
  assert not mgr._session_dir(session.id).exists()

  event = {"type": "user", "content": "must not land"}
  with patch(BROADCAST_PATCH_TARGET, new=AsyncMock()):
    delivered = await mgr.deliver_to_successor(session.id, event)

  assert delivered is None
  assert not (cfg.sessions_dir / session.id).exists()


@pytest.mark.asyncio
async def test_deliver_to_successor_reresolves_when_successor_appears_between_resolve_and_lock(
    tmp_path: Path,
) -> None:
  mgr = SessionManager(CharlieBotConfig(charliebot_home=tmp_path / "home"))
  gen0 = await _make_parent(mgr)
  gen1 = await mgr.create_session(CreateSessionRequest(name="Late successor"), backend="claude-opus-4.6")

  real_read = mgr.read_metadata_fresh
  calls = {"n": 0}

  async def flaky_read(session_id: str):
    if session_id == gen0:
      calls["n"] += 1
      meta = await real_read(session_id)
      # The first read (chain resolution) sees no successor; the second read
      # (under the lock) sees one — as if an elone landed while we waited.
      if calls["n"] >= 2:
        meta.successor_session_id = gen1.id
        await mgr.save_metadata(meta)
      return meta
    return await real_read(session_id)

  event = {"type": "user", "content": "lands in newest tail"}
  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch.object(mgr, "read_metadata_fresh", side_effect=flaky_read),
  ):
    delivered = await mgr.deliver_to_successor(gen0, event)

  assert delivered == gen1.id
  assert event.get("origin_session_id") == gen0
  assert any(ev.get("content") == "lands in newest tail" for ev in mgr.load_chat_events_sync(gen1.id))


# ---------------------------------------------------------------------------
# Scheduler-owned elone: inheriting succession
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_alignment_scan_is_noop_at_every_succession_transition(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """At each transition state the backend alignment scan has no rotation work to do."""
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  _seed_scheduled_task(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr)
  old_backend = parent.backend
  new_backend = "codex-o3"

  async def assert_scan_noop(configured_backend: str, expected_id: str) -> None:
    """The scan returns the already-existing session, and creates/archives nothing."""
    dirs = _session_dir_names(cfg)
    statuses = {s.id: s.status for s in await mgr.list_sessions(scheduled=True)}
    resolved = await mgr.ensure_scheduled_session_backend("nightly", configured_backend)
    assert resolved is not None
    assert resolved.id == expected_id
    assert _session_dir_names(cfg) == dirs
    assert {s.id: s.status for s in await mgr.list_sessions(scheduled=True)} == statuses

  store = mgr._scheduled_sessions
  real_write = store.write_scheduled_task_backend
  seen: dict[str, str] = {}

  async def probed_write(task_name: str, backend: str) -> None:
    # Before write-back: the yaml still names the old backend and the still-active
    # parent matches it, so the scan returns the parent and rotates nothing.
    await assert_scan_noop(old_backend, parent.id)
    await real_write(task_name, backend)
    # After write-back, before the parent is archived: the successor — already
    # active on the written-back backend — matches, so the scan is still a no-op.
    active = await mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
    successor_id = next(s.id for s in active if s.id != parent.id)
    seen["successor_id"] = successor_id
    await assert_scan_noop(new_backend, successor_id)

  monkeypatch.setattr(store, "write_scheduled_task_backend", probed_write)

  child = await mgr.elone_session(parent.id, event_index=1, backend=new_backend)
  assert seen["successor_id"] == child.id
  # After the parent is archived only the successor remains active, matching the
  # written-back backend: the scan still creates/archives nothing.
  await assert_scan_noop(new_backend, child.id)


@pytest.mark.asyncio
async def test_succession_keeps_scheduler_chain_intact_for_tick_and_triggers(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  yaml_path = _seed_scheduled_task(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr)
  trigger_mgr = TriggerManager(cfg, mgr)
  scheduler = Scheduler(cfg, mgr)

  with (
      patch(BROADCAST_PATCH_TARGET, new=AsyncMock()),
      patch(TRIGGER_MASTER_PATCH_TARGET, new=AsyncMock()) as mock_master,
      patch(TRIGGERS_GET_CONFIG_PATCH_TARGET, return_value=cfg),
  ):
    # A trigger stays pending on the parent across the succession.
    trigger = await trigger_mgr.create_trigger(parent.id, delay_seconds=3600, message="queued wake")
    trigger_mgr._tasks[trigger.id].cancel()

    child = await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")

    # The tick's session resolution loads the written-back task yaml and lands
    # on the successor — same task, no forked generation.
    task_cfg, _ = _load_cron_file(yaml_path, cfg.charlie_bot_repo, "nightly")
    resolved = await scheduler._get_or_create_session(task_cfg, cfg, mgr)
    assert resolved is not None
    assert resolved.id == child.id

    # The pending trigger fires through the successor chain into the child.
    stored = await trigger_mgr._load_trigger(parent.id, trigger.id)
    stored.fire_at = datetime.now(UTC) - timedelta(seconds=1)
    await trigger_mgr._save_trigger(stored)
    await trigger_mgr._wait_and_fire(stored)

    stored = await trigger_mgr._load_trigger(parent.id, trigger.id)
    assert stored.status == TriggerStatus.FIRED
    wake = next(ev for ev in mgr.load_chat_events_sync(child.id) if "queued wake" in ev.get("content", ""))
    assert wake.get("origin_session_id") == parent.id
    mock_master.assert_awaited_once()
    assert mock_master.await_args.args[0] == parent.id


@pytest.mark.asyncio
async def test_handoff_reference_holds_exact_parent_prefix_and_parent_pointer(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  _seed_scheduled_task(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr, events=3)

  child = await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")

  reference_path = mgr.get_chat_events_path(child.id).parent / "parent_reference.jsonl"
  reference = [json.loads(line) for line in reference_path.read_text(encoding="utf-8").splitlines()]
  parent_events = mgr.load_chat_events_sync(parent.id)
  assert reference == parent_events[:2]
  assert child.parent_session_id == parent.id


@pytest.mark.asyncio
async def test_busy_scheduler_owned_elone_raises_the_rotation_exception_type(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  _seed_scheduled_task(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr)
  mark_busy(parent.id)
  try:
    with pytest.raises(ScheduledSessionBusyError) as elone_err:
      await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")
    with pytest.raises(ScheduledSessionBusyError) as rotation_err:
      await mgr.ensure_scheduled_session_backend("nightly", "codex-o3")
    assert type(elone_err.value) is type(rotation_err.value)
  finally:
    clear_busy(parent.id)

  # The refusal happens before any mutation: no child, parent untouched.
  assert {s.id for s in await mgr.list_sessions(scheduled=True)} == {parent.id}
  fresh_parent = await mgr.read_metadata_fresh(parent.id)
  assert fresh_parent is not None
  assert fresh_parent.status == SessionStatus.ACTIVE
  assert fresh_parent.successor_session_id is None


@pytest.mark.asyncio
async def test_busy_scheduler_owned_elone_endpoint_maps_to_409(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  _seed_scheduled_task(tmp_path, monkeypatch)
  client = _build_client(cfg, mgr)
  parent = await _make_scheduled_parent(mgr)
  mark_busy(parent.id)
  try:
    with client:
      resp = client.post(f"/api/sessions/{parent.id}/elone", json={"event_index": 0})
  finally:
    clear_busy(parent.id)
  assert resp.status_code == 409
  detail = resp.json()["detail"]
  assert "running work" in detail
  assert "retry" in detail


@pytest.mark.asyncio
async def test_failed_write_back_rolls_back_the_succession(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  yaml_path = _seed_scheduled_task(tmp_path, monkeypatch)
  parent = await _make_scheduled_parent(mgr)
  original_yaml = yaml_path.read_text(encoding="utf-8")

  write_failure = OSError("forced yaml write failure")

  def boom(path: Path, data: dict, **_kwargs) -> None:
    raise write_failure

  monkeypatch.setattr("src.core.scheduled_sessions.save_yaml", boom)

  with pytest.raises(OSError) as excinfo:
    await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")
  assert excinfo.value is write_failure

  # The parent is untouched: active, no successor pointer.
  fresh_parent = await mgr.read_metadata_fresh(parent.id)
  assert fresh_parent is not None
  assert fresh_parent.status == SessionStatus.ACTIVE
  assert fresh_parent.successor_session_id is None
  # No new scheduled session remains registered; the brief successor is archived.
  active = await mgr.list_sessions(status=SessionStatus.ACTIVE, scheduled=True)
  assert [s.id for s in active] == [parent.id]
  assert all(
      s.id == parent.id or s.status == SessionStatus.ARCHIVED
      for s in await mgr.list_sessions(scheduled=True))
  # ...and the task yaml was never rewritten.
  assert yaml_path.read_text(encoding="utf-8") == original_yaml


async def _make_recently_run_cadence_parent(mgr: SessionManager) -> SessionMetadata:
  """A scheduled parent whose last run sits inside the current per-minute window."""
  parent = await _make_scheduled_parent(mgr)
  parent.last_scheduled_run = datetime.now(ZoneInfo("America/Los_Angeles")).isoformat()
  parent.last_scheduled_cron = _CADENCE_CRON
  parent.last_run_status = "success"
  await mgr.save_metadata(parent)
  return parent


def _cadence_task_cfg() -> ScheduledTaskConfig:
  return ScheduledTaskConfig(name="nightly", cron=_CADENCE_CRON, prompt="run nightly", backend="codex-o3")


@pytest.mark.asyncio
async def test_cadence_continuity_tick_does_not_refire_after_succession(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  _seed_scheduled_task(tmp_path, monkeypatch, cron=_CADENCE_CRON)
  parent = await _make_recently_run_cadence_parent(mgr)
  await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")

  scheduler = Scheduler(cfg, mgr)
  execute_task = AsyncMock()
  monkeypatch.setattr(scheduler, "_execute_task", execute_task)

  await scheduler._maybe_run(_cadence_task_cfg(), mgr, None, cfg)

  # Migrated bookkeeping keeps the cadence: the just-run task is not due yet.
  execute_task.assert_not_awaited()


@pytest.mark.asyncio
async def test_cadence_canary_without_bookkeeping_migration_the_tick_refires(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """Companion canary: with the bookkeeping migration removed, the same tick fires."""
  cfg = build_two_backend_cfg(tmp_path)
  mgr = SessionManager(cfg)
  _seed_scheduled_task(tmp_path, monkeypatch, cron=_CADENCE_CRON)
  parent = await _make_recently_run_cadence_parent(mgr)
  monkeypatch.setattr(
      ScheduledSessionStore,
      "migrate_scheduler_bookkeeping",
      lambda self, old_session, new_session: None)

  child = await mgr.elone_session(parent.id, event_index=1, backend="codex-o3")
  fresh_child = await mgr.read_metadata_fresh(child.id)
  assert fresh_child is not None
  assert fresh_child.last_scheduled_run is None  # the migration really was skipped

  scheduler = Scheduler(cfg, mgr)
  execute_task = AsyncMock()
  monkeypatch.setattr(scheduler, "_execute_task", execute_task)

  await scheduler._maybe_run(_cadence_task_cfg(), mgr, None, cfg)

  # Missing last_scheduled_run: the never-run branch reaches back over the
  # current minute window and the task fires immediately.
  execute_task.assert_awaited_once()
