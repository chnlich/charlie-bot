from __future__ import annotations

import os
from collections.abc import Callable
from pathlib import Path

import pytest
from conftest import make_home_config

import src.core.sessions as sessions_mod
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


def _counting_scan(monkeypatch: pytest.MonkeyPatch) -> Callable[[], int]:
  """Count _scan_content_for_hit invocations; delegates to the real scan."""
  calls = 0
  real_scan = sessions_mod._scan_content_for_hit

  def _wrapped(path, session_id, query_lower, start):
    nonlocal calls
    calls += 1
    return real_scan(path, session_id, query_lower, start)

  monkeypatch.setattr(sessions_mod, "_scan_content_for_hit", _wrapped)
  return lambda: calls


def _recording_starts(monkeypatch: pytest.MonkeyPatch) -> list[int]:
  """Record the start offset of each _scan_content_for_hit call; delegates to the real scan."""
  starts: list[int] = []
  real_scan = sessions_mod._scan_content_for_hit

  def _wrapped(path, session_id, query_lower, start):
    starts.append(start)
    return real_scan(path, session_id, query_lower, start)

  monkeypatch.setattr(sessions_mod, "_scan_content_for_hit", _wrapped)
  return starts


@pytest.mark.asyncio
async def test_content_search_miss_memo_skips_rereads(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  session = await _session_with_chat_content(mgr, '{"type":"user","content":"nothing relevant"}\n', "sess")
  count = _counting_scan(monkeypatch)

  assert await mgr.search_sessions("absent") == []
  assert count() == 1
  # Repeat of the memoized needle, and a superstring of it (the debounced
  # sidebar's growing query), both serve the proven-absent memo without a read.
  assert await mgr.search_sessions("absent") == []
  assert await mgr.search_sessions("ABSENT NEEDLE") == []
  assert count() == 1

  # A shorter needle is not covered by the longer memo entry: one rescan, then
  # the shorter entry replaces and covers both.
  assert await mgr.search_sessions("abs") == []
  assert count() == 2
  assert await mgr.search_sessions("absent needle") == []
  assert count() == 2

  # An append moves the (mtime_ns, size, ino) signature; the same-inode growth
  # re-proves from the appended tail, where the needle now sits, so the hit
  # is found.
  events_path = mgr.get_chat_events_path(session.id)
  with events_path.open("a", encoding="utf-8") as stream:
    stream.write('{"type":"user","content":"ABSENT NEEDLE now present"}\n')
  [found] = await mgr.search_sessions("absent needle")
  assert found.id == session.id
  assert count() == 3


@pytest.mark.asyncio
async def test_content_search_errored_scan_is_not_memoized(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  await _session_with_chat_content(mgr, '{"type":"user","content":"nothing relevant"}\n', "sess")
  real_scan = sessions_mod._scan_content_for_hit
  calls = 0

  def _flaky(path, session_id, query_lower, start):
    nonlocal calls
    calls += 1
    if calls == 1:
      return None  # errored scan: no absence proof
    return real_scan(path, session_id, query_lower, start)

  monkeypatch.setattr(sessions_mod, "_scan_content_for_hit", _flaky)

  assert await mgr.search_sessions("absent") == []
  # The errored round memoized nothing, so the identical query scans again.
  assert await mgr.search_sessions("absent") == []
  assert calls == 2
  # The second scan was clean and memoized: the third query is covered.
  assert await mgr.search_sessions("absent") == []
  assert calls == 2


@pytest.mark.asyncio
async def test_content_search_append_rescans_only_the_tail(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  session = await _session_with_chat_content(mgr, '{"type":"user","content":"nothing relevant at all"}\n', "sess")
  starts = _recording_starts(monkeypatch)

  assert await mgr.search_sessions("absent") == []
  assert starts == [0]

  events_path = mgr.get_chat_events_path(session.id)
  size_before = events_path.stat().st_size
  with events_path.open("a", encoding="utf-8") as stream:
    stream.write('{"type":"assistant","content":"still nothing"}\n')
  # Same-inode growth re-proves absence from a window over the appended tail
  # instead of a full reread; the once-proven query is then memo-covered again.
  assert await mgr.search_sessions("absent") == []
  assert starts[-1] == size_before - (4 * len("absent") + 8)
  assert await mgr.search_sessions("absent") == []
  assert len(starts) == 2


@pytest.mark.asyncio
async def test_content_search_tail_rescan_finds_hit_straddling_the_append_boundary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  session = await _session_with_chat_content(mgr, '{"type":"user","content":"just abs"}\nabs', "sess")
  starts = _recording_starts(monkeypatch)

  assert await mgr.search_sessions("absent") == []
  events_path = mgr.get_chat_events_path(session.id)
  size_before = events_path.stat().st_size
  with events_path.open("a", encoding="utf-8") as stream:
    stream.write("ent needle completes here\n")
  # The hit starts three bytes before the old size and ends in the append: the
  # tail window begins at most 4*len(query) bytes back and sees it whole.
  [found] = await mgr.search_sessions("absent")
  assert found.id == session.id
  assert starts[-1] == size_before - (4 * len("absent") + 8)


@pytest.mark.asyncio
async def test_content_search_atomic_rewrite_rescans_the_whole_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  session = await _session_with_chat_content(mgr, "p" * 512 + "\n", "sess")
  starts = _recording_starts(monkeypatch)

  assert await mgr.search_sessions("needle") == []
  events_path = mgr.get_chat_events_path(session.id)
  tmp = events_path.with_name(events_path.name + ".tmp")
  tmp.write_text("needle near the head\n" + "q" * 700 + "\n", encoding="utf-8")
  os.replace(tmp, events_path)  # inode swap, larger size: the old prefix is unproven
  [found] = await mgr.search_sessions("needle")
  assert found.id == session.id
  assert starts[-1] == 0
