from __future__ import annotations

from pathlib import Path

import pytest
from conftest import make_home_config

from src.core.models import CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager


async def _session_with_chat_content(session_mgr: SessionManager, body: str, name: str) -> SessionMetadata:
  session = await session_mgr.create_session(CreateSessionRequest(name=name))
  events_path = session_mgr.get_chat_events_path(session.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(body, encoding="utf-8")
  return session


@pytest.mark.asyncio
async def test_content_search_hits_and_misses(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("src.core.sessions._SEARCH_CHUNK_CHARS", 16)
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  hit = await _session_with_chat_content(mgr, '{"type":"user","content":"the Purple Fox jumped"}\n', "hit-session")
  await _session_with_chat_content(mgr, '{"type":"user","content":"nothing relevant"}\n', "miss-session")

  [found_hit] = await mgr.search_sessions("purple fox")
  assert found_hit.id == hit.id
  assert await mgr.search_sessions("absent needle") == []


@pytest.mark.asyncio
async def test_content_search_matches_across_chunk_boundaries(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("src.core.sessions._SEARCH_CHUNK_CHARS", 16)
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  # The needle starts inside one chunk and ends inside the next.
  needle = "straddleneedle"
  session = await _session_with_chat_content(mgr, "x" * 14 + needle + "y" * 30, "straddle-session")

  [found] = await mgr.search_sessions(needle)
  assert found.id == session.id


@pytest.mark.asyncio
async def test_content_search_needle_longer_than_one_chunk(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("src.core.sessions._SEARCH_CHUNK_CHARS", 16)
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  needle = "a" * 5 + "verylongneedlethatoutlivesachunk" + "b" * 5
  session = await _session_with_chat_content(mgr, "z" * 3 + needle + "z" * 3, "long-needle-session")

  [found] = await mgr.search_sessions(needle)
  assert found.id == session.id
