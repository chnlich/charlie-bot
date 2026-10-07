"""Archived listing: keyset pagination, group aggregates, and the cache-authority mechanisms.

The mechanism assertions pin the design's acceptance terms: after the cache is
warm, list request paths read zero session metadata.json files; archived cache
entries never expire while active entries keep the TTL; the boot scan warms the
cache for every status.
"""

from __future__ import annotations

import time
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import (
    count_path_read_text,
    make_session_mgr,
    user_event,
)

from src.infra.models import SessionMetadata, SessionStatus
from src.runtime.sessions import SessionManager

_BASE_TIME = datetime(2026, 8, 1, 12, 0, 0, tzinfo=UTC)


async def _add_session(
    mgr: SessionManager,
    name: str,
    *,
    status: SessionStatus = SessionStatus.ARCHIVED,
    group: str | None = None,
    minutes: int = 0,
    session_id: str | None = None,
) -> SessionMetadata:
  meta = SessionMetadata(name=name, status=status, group=group, updated_at=_BASE_TIME + timedelta(minutes=minutes))
  if session_id is not None:
    meta.id = session_id
  await mgr.save_metadata(meta)
  return meta


def _count_session_metadata_reads(monkeypatch: pytest.MonkeyPatch, sessions_dir: Path) -> list[Path]:
  """Count reads of sessions/<id>/metadata.json (thread metadata.json files are excluded)."""
  return count_path_read_text(
      monkeypatch, lambda path: path.name == "metadata.json" and path.parent.parent == sessions_dir)


@pytest.mark.asyncio
async def test_keyset_pages_walk_newest_first(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  ordered = [await _add_session(mgr, f"s{i}", minutes=i) for i in range(5)]

  first = await mgr.list_archived_page(limit=2)
  assert [s.id for s in first["sessions"]] == [ordered[4].id, ordered[3].id]
  assert first["has_more"] is True
  assert first["next_before"] == ordered[3].updated_at.isoformat()
  assert first["next_before_id"] == ordered[3].id

  second = await mgr.list_archived_page(limit=2, before=first["next_before"], before_id=first["next_before_id"])
  assert [s.id for s in second["sessions"]] == [ordered[2].id, ordered[1].id]
  assert second["has_more"] is True

  third = await mgr.list_archived_page(limit=2, before=second["next_before"], before_id=second["next_before_id"])
  assert [s.id for s in third["sessions"]] == [ordered[0].id]
  assert third["has_more"] is False
  assert third["next_before"] is None
  assert third["next_before_id"] is None


@pytest.mark.asyncio
async def test_bad_cursor_fails_loudly(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  await _add_session(mgr, "s0")

  with pytest.raises(ValueError, match="not-a-timestamp"):
    await mgr.list_archived_page(before="not-a-timestamp", before_id="x")
  with pytest.raises(ValueError, match="timezone-aware"):
    await mgr.list_archived_page(before="2026-08-01T12:00:00", before_id="x")  # naive timestamp
  with pytest.raises(ValueError, match="pass both or neither"):
    await mgr.list_archived_page(before=_BASE_TIME.isoformat(), before_id=None)  # half a cursor


@pytest.mark.asyncio
async def test_warm_list_paths_read_zero_metadata_files(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mgr = make_session_mgr(tmp_path)
  for i in range(3):
    await _add_session(mgr, f"arch{i}", minutes=i)
  await _add_session(mgr, "live-alpha", status=SessionStatus.ACTIVE, minutes=10)

  await mgr.list_sessions()  # warm every entry

  reads = _count_session_metadata_reads(monkeypatch, mgr._cfg.sessions_dir)
  await mgr.list_archived_page(limit=2)
  await mgr.list_sessions(status=SessionStatus.ACTIVE)
  await mgr.search_sessions("alpha")
  assert reads == []


@pytest.mark.asyncio
async def test_archived_entries_survive_ttl_active_entries_expire(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  mgr = make_session_mgr(tmp_path)
  archived = await _add_session(mgr, "old", minutes=0)
  active = await _add_session(mgr, "live", status=SessionStatus.ACTIVE, minutes=1)

  # Age every cache entry past the TTL without touching the clock machinery.
  # Signature None: the expired entry cannot revalidate by stat and re-reads.
  for sid, (meta, _ts, _sig) in list(mgr._metadata_cache.items()):
    mgr._metadata_cache[sid] = (meta, time.monotonic() - 3600, None)

  reads = _count_session_metadata_reads(monkeypatch, mgr._cfg.sessions_dir)
  listed = await mgr.list_sessions()
  assert {s.id for s in listed} == {archived.id, active.id}
  assert [p.parent.name for p in reads] == [active.id]


@pytest.mark.asyncio
async def test_search_cap_keeps_content_hits_above_the_cap_line(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  for i in range(200):
    await _add_session(mgr, f"needle-{i:03d}", minutes=i)
  above = await _add_session(mgr, "unrelated-a", status=SessionStatus.ACTIVE, minutes=1000)
  below = await _add_session(mgr, "unrelated-b", status=SessionStatus.ACTIVE, minutes=-1)
  await mgr.save_chat_event(above.id, user_event("needle in the events"))
  await mgr.save_chat_event(below.id, user_event("needle in the events"))

  rows, derived = await mgr.search_sessions_readonly(
      "needle", include_running_status=True, include_pending_trigger_status=True)
  ids = [r.id for r in rows]
  assert len(rows) == 200
  assert above.id in ids  # a hit newer than the cap line displaces the oldest match
  assert below.id not in ids  # a hit older than every match cannot enter the top rows
  assert set(derived) == set(ids)

  sessions = await mgr.search_sessions("needle", include_running_status=True, include_pending_trigger_status=True)
  assert [s.id for s in sessions] == ids  # the copying wrapper serves the same rows


@pytest.mark.asyncio
async def test_search_match_memo_refreshes_when_chat_content_or_names_move(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  await _add_session(mgr, "quiet", status=SessionStatus.ACTIVE, minutes=1)
  carrier = await _add_session(mgr, "carrier", status=SessionStatus.ACTIVE, minutes=2)

  first, _ = await mgr.search_sessions_readonly(
      "needle", include_running_status=False, include_pending_trigger_status=False)
  assert first == []
  repeat, _ = await mgr.search_sessions_readonly(
      "needle", include_running_status=False, include_pending_trigger_status=False)
  assert repeat is first  # the unchanged corpus serves the stored rows

  # A chat file moves without touching any metadata.json: the append must
  # re-key the derivation or the new hit stays invisible until the next
  # metadata write.
  await mgr.save_chat_event(carrier.id, user_event("needle in the events"))
  grown, _ = await mgr.search_sessions_readonly(
      "needle", include_running_status=False, include_pending_trigger_status=False)
  assert [r.id for r in grown] == [carrier.id]

  renamed = await _add_session(mgr, "needle-name", minutes=3)
  named, _ = await mgr.search_sessions_readonly(
      "needle", include_running_status=False, include_pending_trigger_status=False)
  assert {r.id for r in named} == {carrier.id, renamed.id}


@pytest.mark.asyncio
async def test_search_match_memo_stores_nothing_after_an_errored_scan(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  import src.runtime.sessions as sessions_module

  mgr = make_session_mgr(tmp_path)
  carrier = await _add_session(mgr, "carrier", status=SessionStatus.ACTIVE, minutes=1)
  await mgr.save_chat_event(carrier.id, user_event("nothing relevant here"))

  real_scan = sessions_module._scan_content_for_hit
  monkeypatch.setattr(sessions_module, "_scan_content_for_hit", lambda *a, **k: None)
  errored, _ = await mgr.search_sessions_readonly(
      "needle", include_running_status=False, include_pending_trigger_status=False)
  assert errored == []  # the errored scan proves no absence, so the rows stay undetermined
  assert mgr._search_match_memo.get("needle") is None  # an errored round stores nothing

  monkeypatch.setattr(sessions_module, "_scan_content_for_hit", real_scan)
  retried, _ = await mgr.search_sessions_readonly(
      "needle", include_running_status=False, include_pending_trigger_status=False)
  assert retried == []  # the retry re-scans instead of serving the errored rows
  assert mgr._search_match_memo.get("needle") is not None  # the clean round stores


@pytest.mark.asyncio
async def test_search_absence_roots_cover_the_whole_candidate_set_across_a_churn_derive(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  import src.runtime.sessions as sessions_module

  # One active chat file per session, the population just past the pre-4096
  # file cap (260 sessions over the 256-entry cap): a cap under the
  # candidate population evicts roots the corpus outgrew, and the next
  # churn derive re-reads every evicted file from byte 0 instead of the
  # appended tail alone.
  mgr = make_session_mgr(tmp_path)
  carriers = [await _add_session(mgr, f"bulk-{i:03d}", status=SessionStatus.ACTIVE, minutes=i) for i in range(260)]
  for meta in carriers:
    await mgr.save_chat_event(meta.id, user_event("filler line\n"))

  needle = "zzq9neverpresentneedle"
  first, _ = await mgr.search_sessions_readonly(
      needle, include_running_status=False, include_pending_trigger_status=False)
  assert first == []
  assert len(list(mgr._search_miss_memo.items())) == len(carriers)

  appended = carriers[0]
  await mgr.save_chat_event(appended.id, user_event("churn append\n"))

  scans: list[tuple[str, int]] = []
  real_scan = sessions_module._scan_content_for_hit

  def spy(path, session_id, query_lower, start):
    scans.append((session_id, start))
    return real_scan(path, session_id, query_lower, start)

  monkeypatch.setattr(sessions_module, "_scan_content_for_hit", spy)
  rows, _ = await mgr.search_sessions_readonly(
      needle, include_running_status=False, include_pending_trigger_status=False)
  assert rows == []
  # The churn derive re-proves the appended file on its tail alone; every
  # unmoved file's stored root still answers without a read.
  assert len(scans) == 1
  assert scans[0][0] == appended.id
  assert scans[0][1] > 0

  ride, _ = await mgr.search_sessions_readonly(
      needle, include_running_status=False, include_pending_trigger_status=False)
  assert ride == []
  assert len(scans) == 1  # the derived rows serve the repeat without a read


def test_content_scan_raw_path_matches_decoded_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  import src.runtime.sessions as sessions_module

  # A tiny window forces many boundary carries, so the straddle cases run for
  # real instead of riding one whole-file window.
  monkeypatch.setattr(sessions_module, "_SEARCH_CHUNK_SIZE", 4)
  chat = tmp_path / "chat_events.jsonl"
  prefix = "xxNeeDLe\u00e9tail".encode()  # mixed-case hit; \u00e9 must not disturb it
  chat.write_bytes(prefix + ("filler" * 10).encode())
  assert sessions_module._scan_content_for_hit(chat, "s", "needle", 0) is True
  assert sessions_module._scan_content_for_hit(chat, "s", "zzq9absent", 0) is False
  # The rescan contract: a start offset past the only hit hides it, and an
  # append past that offset shows the new hit -- the memo's re-proof window.
  assert sessions_module._scan_content_for_hit(chat, "s", "needle", len(prefix)) is False
  with chat.open("ab") as out:
    out.write(b"TailNeedle")
  assert sessions_module._scan_content_for_hit(chat, "s", "needle", len(prefix)) is True
  # The documented boundary: an ASCII needle does not match U+212A (whose
  # str.lower() contains an ASCII letter); that codepoint needs the decoded
  # path, which a non-ASCII needle rides.
  kelvin = tmp_path / "kelvin.jsonl"
  kelvin.write_bytes("\u212a".encode() + b"ey")
  assert sessions_module._scan_content_for_hit(kelvin, "s", "k", 0) is False
  accents = tmp_path / "accents.jsonl"
  accents.write_bytes("caf\u00e9".encode())
  assert sessions_module._scan_content_for_hit(accents, "s", "\u00e9", 0) is True


@pytest.mark.asyncio
async def test_boot_scan_warms_cache_for_every_status(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  archived = await _add_session(mgr, "cold-archived", minutes=0)
  active = await _add_session(mgr, "cold-active", status=SessionStatus.ACTIVE, minutes=1)

  rebooted = SessionManager(mgr._cfg)
  listed = rebooted.list_active_session_metas()

  assert [s.id for s in listed] == [active.id]
  assert archived.id in rebooted._metadata_cache
  assert rebooted._metadata_cache[archived.id][0].status == SessionStatus.ARCHIVED
