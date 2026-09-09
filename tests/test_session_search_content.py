from __future__ import annotations

import os
from collections.abc import Callable
from pathlib import Path

import pytest
from conftest import fresh_state_fixture, make_home_config

import src.api.sessions as sessions_api
import src.core.sessions as sessions_mod
from src.api.responses import fast_json_bytes
from src.core import thinking_state
from src.core.models import CreateSessionRequest, SessionMetadata
from src.core.sessions import SessionManager

_fresh_search_read_failure_registry = fresh_state_fixture(sessions_mod._reset_search_read_failures_for_tests)


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

  # A shorter needle is not covered by the longer root: one rescan joins it as
  # its own root, and both families now serve without a read.
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
async def test_content_search_miss_memo_serves_independent_needle_families(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  await _session_with_chat_content(mgr, '{"type":"user","content":"nothing relevant"}\n', "sess")
  count = _counting_scan(monkeypatch)

  # Two unrelated families cold-scan once each.
  assert await mgr.search_sessions("alpha") == []
  assert await mgr.search_sessions("omega") == []
  assert count() == 2
  # Repeats of either family serve from their own root: a needle outside the
  # other family's superstrings must never evict it back into full rescans.
  for _ in range(3):
    assert await mgr.search_sessions("alpha") == []
    assert await mgr.search_sessions("omega") == []
  assert count() == 2


@pytest.mark.asyncio
async def test_content_search_miss_memo_root_cap_is_per_file_lru(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setattr("src.core.sessions._SEARCH_MISS_ROOTS_PER_FILE", 2)
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  await _session_with_chat_content(mgr, '{"type":"user","content":"nothing relevant"}\n', "sess")
  count = _counting_scan(monkeypatch)

  assert await mgr.search_sessions("one") == []
  assert await mgr.search_sessions("two") == []
  assert count() == 2
  # A third family evicts the oldest root ("one"); "two" still serves.
  assert await mgr.search_sessions("three") == []
  assert count() == 3
  assert await mgr.search_sessions("two") == []
  assert count() == 3
  # The evicted family rescans once, rejoins as the newest root, and serves again.
  assert await mgr.search_sessions("one") == []
  assert count() == 4
  assert await mgr.search_sessions("one") == []
  assert await mgr.search_sessions("three") == []
  assert count() == 4


@pytest.mark.asyncio
async def test_content_search_memoize_landing_mid_round_does_not_disturb_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  await _session_with_chat_content(mgr, '{"type":"user","content":"nothing relevant"}\n', "sess")
  real_scan = sessions_mod._scan_content_for_hit

  def _concurrent_memorizing_scan(path, session_id, query_lower, start):
    # A concurrent search's memoize lands on the same file while this round's
    # worker is inside its scan (the per-keystroke sidebar's overlapping
    # requests); the in-flight round reads its snapshot and completes.
    st = path.stat()
    mgr._memoize_search_miss(str(path), (st.st_mtime_ns, st.st_size, st.st_ino), "concurrent")
    return real_scan(path, session_id, query_lower, start)

  monkeypatch.setattr(sessions_mod, "_scan_content_for_hit", _concurrent_memorizing_scan)

  assert await mgr.search_sessions("absent") == []
  assert await mgr.search_sessions("absent") == []


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
async def test_content_search_missing_chat_file_logs_once_per_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  # A fresh session's data/ holds no live chat file, so every content scan's
  # stat fails; the once-guard keeps the round's first sighting only.
  await mgr.create_session(CreateSessionRequest(name="no-events-yet"))
  events: list[str] = []
  monkeypatch.setattr(sessions_mod.log, "debug", lambda event, **kw: events.append(event))

  assert await mgr.search_sessions("absent") == []
  assert events == ["search_read_failed"]
  for _ in range(3):
    assert await mgr.search_sessions("absent") == []
  assert events == ["search_read_failed"]

  sessions_mod._reset_search_read_failures_for_tests()
  assert await mgr.search_sessions("absent") == []
  assert events == ["search_read_failed", "search_read_failed"]


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


@pytest.mark.asyncio
async def test_search_route_body_is_byte_identical_to_the_merged_render(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  await _session_with_chat_content(mgr, '{"type":"user","content":"irrelevant"}\n', "needle-ascii")
  await _session_with_chat_content(mgr, '{"type":"user","content":"irrelevant"}\n', "针-needle-会话")

  rows, derived = await mgr.search_sessions_readonly(
      "needle", include_running_status=True, include_pending_trigger_status=True)
  assert len(rows) == 2
  payload = []
  for meta in rows:
    entry = derived[meta.id]
    row = meta.model_dump(mode="json")
    row["thinking_since"] = sessions_api._UTC_DATETIME_JSON.dump_python(thinking_state.busy_since(meta.id), mode="json")
    row["has_running_tasks"] = entry["has_running_tasks"]
    row["has_pending_trigger"] = entry["has_pending_trigger"]
    row["pending_trigger_count"] = entry["pending_trigger_count"]
    row["next_trigger_at"] = sessions_api._UTC_DATETIME_JSON.dump_python(entry["next_trigger_at"], mode="json")
    payload.append(row)

  first = await sessions_api.search_sessions(q="needle", session_mgr=mgr)
  second = await sessions_api.search_sessions(q="needle", session_mgr=mgr)
  assert first.body == fast_json_bytes(payload)
  assert second.body == first.body  # the memo serves the same bytes


@pytest.mark.asyncio
async def test_search_row_memo_follows_the_write_funnel_rename(tmp_path: Path) -> None:
  cfg = make_home_config(tmp_path)
  mgr = SessionManager(cfg)
  session = await _session_with_chat_content(mgr, '{"type":"user","content":"irrelevant"}\n', "needle-v1")

  first = await sessions_api.search_sessions(q="needle", session_mgr=mgr)
  assert b"needle-v1" in first.body

  await mgr.rename_session(session.id, "needle-v2")  # save_metadata replaces the cached object
  second = await sessions_api.search_sessions(q="needle", session_mgr=mgr)
  assert b"needle-v2" in second.body
  assert b"needle-v1" not in second.body


def test_search_row_memo_respects_its_cap() -> None:
  sessions_api._search_row_fragments.clear()
  metas = [SessionMetadata.model_construct(id=f"s{i}", name=f"needle-{i}") for i in range(600)]
  for meta in metas:
    sessions_api._search_row_static_segments(meta)
  assert len(sessions_api._search_row_fragments) == sessions_api._SEARCH_ROW_FRAGMENT_CAP
  # The evicted oldest entry is gone; a recent one survives with its pinned object.
  assert sessions_api._search_row_fragments[id(metas[599])][0] is metas[599]
  sessions_api._search_row_fragments.clear()
