"""Tests for the elone succession pointer and successor-chain resolution."""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig, get_config
from src.core.models import BackendOption, CreateSessionRequest, SessionStatus
from src.core.sessions import SessionManager, SuccessionRefused


def _append_events(path: Path, events: list[dict]) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    for event in events:
      f.write(json.dumps(event) + "\n")


def _session_dir_names(cfg: CharlieBotConfig) -> set[str]:
  """Snapshot the names of session directories on disk (existence, not content)."""
  if not cfg.sessions_dir.exists():
    return set()
  return {d.name for d in cfg.sessions_dir.iterdir() if d.is_dir()}


async def _make_parent(mgr: SessionManager, *, name: str = "Parent") -> str:
  parent = await mgr.create_session(CreateSessionRequest(name=name), backend="claude-opus-4.6")
  _append_events(
      mgr.get_chat_events_path(parent.id),
      [
          {"type": "user", "content": "e0"},
          {"type": "assistant", "content": "e1"},
      ],
  )
  return parent.id


def _build_client(cfg: CharlieBotConfig, session_mgr: SessionManager) -> TestClient:
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_config] = lambda: cfg
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return TestClient(app)


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
      ],
  )


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
async def test_second_elone_of_same_parent_raises_and_creates_no_child(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent_id = await _make_parent(mgr)

  await mgr.elone_session(parent_id, event_index=0)
  before = _session_dir_names(cfg)

  with pytest.raises(SuccessionRefused):
    await mgr.elone_session(parent_id, event_index=0)
  assert _session_dir_names(cfg) == before

  fresh_parent = await mgr.read_metadata_fresh(parent_id)
  assert fresh_parent is not None
  assert fresh_parent.successor_session_id is not None


@pytest.mark.asyncio
async def test_elone_of_scheduler_owned_session_raises_and_creates_no_child(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  scheduled = await mgr.create_session(
      CreateSessionRequest(name="Scheduled", scheduled_task="nightly"), backend="claude-opus-4.6")

  before = _session_dir_names(cfg)
  with pytest.raises(SuccessionRefused, match="scheduler"):
    await mgr.elone_session(scheduled.id, event_index=0)
  assert _session_dir_names(cfg) == before


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

  for current, successor in zip(sessions, sessions[1:]):
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
async def test_resolve_successor_chain_returns_none_for_missing_session(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  assert await mgr.resolve_successor_chain("does-not-exist") is None


@pytest.mark.asyncio
async def test_resolve_successor_chain_raises_runtime_error_on_cycle(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  a = await mgr.create_session(CreateSessionRequest(name="A"), backend="claude-opus-4.6")
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
async def test_api_succession_refused_maps_to_409_and_bad_event_index_to_400(tmp_path: Path) -> None:
  cfg = _build_cfg(tmp_path)
  mgr = SessionManager(cfg)
  client = _build_client(cfg, mgr)

  parent_id = await _make_parent(mgr)

  # First elone succeeds.
  with client:
    resp = client.post(f"/api/sessions/{parent_id}/elone", json={"event_index": 0})
    assert resp.status_code == 200

    # Second elone of the same parent: SuccessionRefused -> 409.
    resp = client.post(f"/api/sessions/{parent_id}/elone", json={"event_index": 0})
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
  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
    delivered = await mgr.deliver_to_successor(gen0, event)

  assert delivered == gen2.id
  assert event.get("origin_session_id") == gen0
  assert any(ev.get("content") == "delivered" for ev in mgr.load_chat_events_sync(gen2.id))


@pytest.mark.asyncio
async def test_deliver_to_successor_leaves_origin_absent_for_no_successor(tmp_path: Path) -> None:
  mgr = SessionManager(CharlieBotConfig(charliebot_home=tmp_path / "home"))
  gen0 = await _make_parent(mgr)

  event = {"type": "user", "content": "no redirect"}
  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
    delivered = await mgr.deliver_to_successor(gen0, event)

  assert delivered == gen0
  assert "origin_session_id" not in event
  assert any(ev.get("content") == "no redirect" for ev in mgr.load_chat_events_sync(gen0))


@pytest.mark.asyncio
async def test_deliver_to_successor_returns_none_and_writes_nothing_when_chain_end_dir_removed(
    tmp_path: Path,
) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Gone"), backend="claude-opus-4.6")

  # Remove the whole session directory, including metadata.json, exactly as a
  # permanent delete does. append_ndjson would recreate the dir — assert it does not.
  await mgr.delete_session_permanently(session.id)
  assert not mgr._session_dir(session.id).exists()

  event = {"type": "user", "content": "must not land"}
  with patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()):
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
      patch("src.core.sessions.streaming_manager.broadcast", new=AsyncMock()),
      patch.object(mgr, "read_metadata_fresh", side_effect=flaky_read),
  ):
    delivered = await mgr.deliver_to_successor(gen0, event)

  assert delivered == gen1.id
  assert event.get("origin_session_id") == gen0
  assert any(ev.get("content") == "lands in newest tail" for ev in mgr.load_chat_events_sync(gen1.id))
