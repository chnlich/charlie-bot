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
    OPUS_BACKEND_ID,
    build_sessions_cfg,
    count_path_read_text,
    make_session_mgr,
)
from conftest import make_sessions_client as _build_client

from src.core.models import CreateSessionRequest, SessionMetadata, SessionStatus
from src.core.sessions import SessionManager

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
async def test_equal_timestamps_tie_break_on_id(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  for sid in ("id-a", "id-b", "id-c"):
    await _add_session(mgr, sid, minutes=0, session_id=sid)

  first = await mgr.list_archived_page(limit=2)
  assert [s.id for s in first["sessions"]] == ["id-c", "id-b"]

  second = await mgr.list_archived_page(limit=2, before=first["next_before"], before_id=first["next_before_id"])
  assert [s.id for s in second["sessions"]] == ["id-a"]
  assert second["has_more"] is False


@pytest.mark.asyncio
async def test_group_filter_and_whole_set_aggregates(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  await _add_session(mgr, "a1", group="alpha", minutes=1)
  await _add_session(mgr, "a2", group="alpha", minutes=2)
  await _add_session(mgr, "b1", group="beta", minutes=3)
  await _add_session(mgr, "u1", minutes=4)
  await _add_session(mgr, "u2", minutes=5)
  await _add_session(mgr, "active", status=SessionStatus.ACTIVE, group="alpha", minutes=6)

  everything = await mgr.list_archived_page()
  assert len(everything["sessions"]) == 5
  assert everything["groups"] == [
      {
          "group": "alpha",
          "total": 2
      },
      {
          "group": "beta",
          "total": 1
      },
      {
          "group": None,
          "total": 2
      },
  ]

  alpha = await mgr.list_archived_page(group="alpha")
  assert sorted(s.name for s in alpha["sessions"]) == ["a1", "a2"]
  # The aggregates cover the whole archived set, not the current filter.
  assert alpha["groups"] == everything["groups"]

  ungrouped = await mgr.list_archived_page(group="")
  assert sorted(s.name for s in ungrouped["sessions"]) == ["u1", "u2"]


@pytest.mark.asyncio
async def test_limit_clamps_to_1_500(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  for i in range(3):
    await _add_session(mgr, f"s{i}", minutes=i)

  floor = await mgr.list_archived_page(limit=0)
  assert len(floor["sessions"]) == 1
  assert floor["has_more"] is True

  ceiling = await mgr.list_archived_page(limit=9999)
  assert len(ceiling["sessions"]) == 3


@pytest.mark.asyncio
async def test_bad_cursor_fails_loudly(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  await _add_session(mgr, "s0")

  with pytest.raises(ValueError):
    await mgr.list_archived_page(before="not-a-timestamp", before_id="x")
  with pytest.raises(ValueError):
    await mgr.list_archived_page(before="2026-08-01T12:00:00", before_id="x")  # naive timestamp
  with pytest.raises(ValueError):
    await mgr.list_archived_page(before=_BASE_TIME.isoformat(), before_id=None)  # half a cursor


@pytest.mark.asyncio
async def test_archived_endpoint_shape_and_cursor_422(tmp_path: Path) -> None:
  cfg = build_sessions_cfg(tmp_path)
  mgr = SessionManager(cfg)
  for i in range(3):
    await _add_session(mgr, f"s{i}", minutes=i)

  with _build_client(cfg, mgr) as client:
    page = client.get("/api/sessions/archived", params={"limit": 2})
    bad = client.get("/api/sessions/archived", params={"before": "garbage", "before_id": "x"})
    filtered = client.get("/api/sessions/archived", params={"group": ""})

  assert page.status_code == 200
  body = page.json()
  assert set(body) == {"sessions", "has_more", "next_before", "next_before_id", "groups"}
  assert len(body["sessions"]) == 2
  assert body["has_more"] is True
  assert bad.status_code == 422
  assert filtered.status_code == 200
  assert len(filtered.json()["sessions"]) == 3


@pytest.mark.asyncio
async def test_archive_unarchive_delete_visible_immediately(tmp_path: Path) -> None:
  cfg = build_sessions_cfg(tmp_path)
  mgr = SessionManager(cfg)
  meta = await mgr.create_session(CreateSessionRequest(name="Journey"), backend=OPUS_BACKEND_ID)
  await mgr.save_chat_event(meta.id, {"type": "user", "content": "hi"})  # non-empty: archive keeps it

  assert await mgr.archive_session(meta.id) is not None
  assert meta.id in {s.id for s in (await mgr.list_archived_page())["sessions"]}

  assert await mgr.unarchive_session(meta.id) is not None
  assert meta.id not in {s.id for s in (await mgr.list_archived_page())["sessions"]}

  await mgr.archive_session(meta.id)
  assert await mgr.delete_session_permanently(meta.id) is True
  assert meta.id not in {s.id for s in (await mgr.list_archived_page())["sessions"]}


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
  for sid, (meta, _ts) in list(mgr._metadata_cache.items()):
    mgr._metadata_cache[sid] = (meta, time.monotonic() - 3600)

  reads = _count_session_metadata_reads(monkeypatch, mgr._cfg.sessions_dir)
  listed = await mgr.list_sessions()
  assert {s.id for s in listed} == {archived.id, active.id}
  assert [p.parent.name for p in reads] == [active.id]


@pytest.mark.asyncio
async def test_search_names_cover_archived_and_cap_at_200(tmp_path: Path) -> None:
  mgr = make_session_mgr(tmp_path)
  for i in range(205):
    await _add_session(mgr, f"needle-{i:03d}", minutes=i)
  await _add_session(mgr, "needle-live", status=SessionStatus.ACTIVE, minutes=999)
  content_only = await _add_session(mgr, "unrelated-name", minutes=998)
  await mgr.save_chat_event(content_only.id, {"type": "user", "content": "needle in the events"})

  results = await mgr.search_sessions("needle")
  assert len(results) == 200
  assert results[0].name == "needle-live"  # newest first survives the cap
  statuses = {s.status for s in results}
  assert SessionStatus.ARCHIVED in statuses
  # Content matches stay active-only: the archived session whose events contain
  # the needle, but whose name does not, is absent.
  assert content_only.id not in {s.id for s in results}


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
