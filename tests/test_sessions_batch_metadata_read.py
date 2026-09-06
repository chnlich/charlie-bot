"""Tests for the batched metadata read used by session listings."""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import time
from pathlib import Path

import pytest
from conftest import count_path_read_text
from conftest import make_session_mgr as _make_session_mgr
from structlog.testing import capture_logs

from src.core import sessions as sessions_module
from src.core.models import SessionMetadata, SessionStatus
from src.core.sessions import SessionManager


def _write_metadata(mgr: SessionManager, meta: SessionMetadata, raw: str | None = None) -> Path:
  path = mgr._metadata_path(meta.id)
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(meta.model_dump_json() if raw is None else raw, encoding="utf-8")
  return path


def _failed_events(logs: list[dict], session_id: str) -> list[dict]:
  return [
      entry for entry in logs if entry.get("event") == "session_load_failed" and entry.get("session_id") == session_id
  ]


@pytest.mark.asyncio
async def test_batch_fills_missing_active_and_archived_metadata(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  active = SessionMetadata(name="active")
  archived = SessionMetadata(name="archived", status=SessionStatus.ARCHIVED)
  _write_metadata(mgr, active)
  _write_metadata(mgr, archived)

  result = await mgr._load_session_metas()

  assert {meta.id for meta in result} == {active.id, archived.id}
  assert mgr._metadata_cache[active.id][0].name == "active"
  assert mgr._metadata_cache[archived.id][0].status == SessionStatus.ARCHIVED


@pytest.mark.asyncio
async def test_batch_skips_metadata_reads_when_cache_is_fresh(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(name="cached")
  await mgr.save_metadata(meta)
  metadata_reads = count_path_read_text(monkeypatch, lambda path: path.name == "metadata.json")

  result = await mgr._load_session_metas()

  assert [item.id for item in result] == [meta.id]
  assert not metadata_reads


@pytest.mark.asyncio
async def test_batch_isolates_file_read_failures_per_session(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  mgr = _make_session_mgr(tmp_path)
  good = SessionMetadata(name="good")
  bad = SessionMetadata(name="bad")
  _write_metadata(mgr, good)
  bad_path = _write_metadata(mgr, bad)
  real_read_text = Path.read_text

  def fail_bad_read(path: Path, *args: object, **kwargs: object) -> str:
    if path == bad_path:
      raise OSError("read failed")
    return real_read_text(path, *args, **kwargs)

  monkeypatch.setattr(Path, "read_text", fail_bad_read)

  with capture_logs() as logs:
    result = await mgr._load_session_metas()

  assert {meta.id for meta in result} == {good.id}
  failures = _failed_events(logs, bad.id)
  assert len(failures) == 1
  assert failures[0]["error"] == "read failed"


@pytest.mark.asyncio
async def test_batch_isolates_model_validate_json_failures_per_session(tmp_path: Path,) -> None:
  mgr = _make_session_mgr(tmp_path)
  good = SessionMetadata(name="good")
  bad = SessionMetadata(name="bad")
  _write_metadata(mgr, good)
  _write_metadata(mgr, bad, "{not valid json")

  with capture_logs() as logs:
    result = await mgr._load_session_metas()

  assert {meta.id for meta in result} == {good.id}
  failures = _failed_events(logs, bad.id)
  assert len(failures) == 1
  assert failures[0]["error"]


@pytest.mark.asyncio
async def test_batch_isolates_migration_save_failures_per_session(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  mgr = _make_session_mgr(tmp_path)
  good = SessionMetadata(name="good")
  bad = SessionMetadata(name="bad", round_ratings={"5": "thumbs_up"})
  _write_metadata(mgr, good)
  _write_metadata(mgr, bad)
  real_save = mgr.save_metadata

  async def fail_bad_save(meta: SessionMetadata) -> None:
    if meta.id == bad.id:
      raise OSError("save failed")
    await real_save(meta)

  monkeypatch.setattr(mgr, "save_metadata", fail_bad_save)

  with capture_logs() as logs:
    result = await mgr._load_session_metas()

  assert {meta.id for meta in result} == {good.id}
  failures = _failed_events(logs, bad.id)
  assert len(failures) == 1
  assert failures[0]["error"] == "save failed"


@pytest.mark.asyncio
async def test_batch_install_uses_setdefault_for_cache_entry_added_during_read(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  mgr = _make_session_mgr(tmp_path)
  disk_meta = SessionMetadata(name="from disk")
  _write_metadata(mgr, disk_meta)
  installed_during_read = SessionMetadata(id=disk_meta.id, name="installed during read")
  real_to_thread = asyncio.to_thread
  to_thread_calls = 0

  async def install_between_diff_and_install(func, *args, **kwargs):
    nonlocal to_thread_calls
    to_thread_calls += 1
    result = await real_to_thread(func, *args, **kwargs)
    if to_thread_calls == 2:
      mgr._metadata_cache[disk_meta.id] = (
          installed_during_read,
          time.monotonic(),
          None,
      )
    return result

  monkeypatch.setattr(sessions_module.asyncio, "to_thread", install_between_diff_and_install)

  result = await mgr._load_session_metas()

  assert to_thread_calls == 2
  assert mgr._metadata_cache[disk_meta.id][0] is installed_during_read
  assert result[0].name == "from disk"


@pytest.mark.asyncio
async def test_batch_migrates_legacy_round_ratings_on_cache_hit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    mgr = _make_session_mgr(tmp_path)
    legacy = SessionMetadata(name="legacy cache hit", round_ratings={"7": "thumbs_up"})
    await mgr.save_metadata(legacy)
    real_save = mgr.save_metadata
    save_calls = 0

    async def counting_save(meta: SessionMetadata) -> None:
        nonlocal save_calls
        save_calls += 1
        await real_save(meta)

    monkeypatch.setattr(mgr, "save_metadata", counting_save)

    result = await mgr._load_session_metas()

    assert save_calls == 1
    assert result[0].round_ratings == {"legacy:7": "thumbs_up"}
    on_disk = json.loads(mgr._metadata_path(legacy.id).read_text(encoding="utf-8"))
    assert on_disk["round_ratings"] == {"legacy:7": "thumbs_up"}


@pytest.mark.asyncio
async def test_batch_migrates_legacy_round_ratings_on_missing_path(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  mgr = _make_session_mgr(tmp_path)
  legacy = SessionMetadata(name="legacy missing", round_ratings={"8": "thumbs_down"})
  _write_metadata(mgr, legacy)
  real_save = mgr.save_metadata
  save_calls = 0

  async def counting_save(meta: SessionMetadata) -> None:
    nonlocal save_calls
    save_calls += 1
    await real_save(meta)

  monkeypatch.setattr(mgr, "save_metadata", counting_save)

  result = await mgr._load_session_metas()

  assert save_calls == 1
  assert result[0].round_ratings == {"legacy:8": "thumbs_down"}
  on_disk = json.loads(mgr._metadata_path(legacy.id).read_text(encoding="utf-8"))
  assert on_disk["round_ratings"] == {"legacy:8": "thumbs_down"}


@pytest.mark.asyncio
async def test_batch_output_matches_sequential_get_session_for_mixed_fixture(tmp_path: Path,) -> None:
  mgr = _make_session_mgr(tmp_path)
  active = SessionMetadata(name="active")
  archived = SessionMetadata(name="archived", status=SessionStatus.ARCHIVED)
  legacy = SessionMetadata(name="legacy", round_ratings={"9": "thumbs_up"})
  corrupt = SessionMetadata(name="corrupt")
  _write_metadata(mgr, active)
  _write_metadata(mgr, archived)
  _write_metadata(mgr, legacy)
  _write_metadata(mgr, corrupt, "{corrupt")

  batch_result = await mgr._load_session_metas()
  sequential_mgr = SessionManager(mgr._cfg)
  sequential_result: list[SessionMetadata] = []
  for session_dir in mgr._cfg.sessions_dir.iterdir():
    if not session_dir.is_dir():
      continue
    try:
      meta = await sequential_mgr.get_session(session_dir.name)
    except Exception:
      continue
    if meta is not None:
      sequential_result.append(meta)

  assert [meta.model_dump(mode="json") for meta in batch_result
         ] == [meta.model_dump(mode="json") for meta in sequential_result]
  assert {meta.id for meta in batch_result} == {active.id, archived.id, legacy.id}


@pytest.mark.asyncio
async def test_dir_names_memo_rescans_only_when_root_changes(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    mgr = _make_session_mgr(tmp_path)
    first = SessionMetadata(name="first")
    _write_metadata(mgr, first)
    await mgr._load_session_metas()

    real_scandir = os.scandir
    root_scans: list[str] = []

    def counting_scandir(path):
        # shutil.rmtree scans by fd; only the sessions-root path counts.
        if isinstance(path, (str, os.PathLike)) and os.fspath(path) == os.fspath(mgr._cfg.sessions_dir):
            root_scans.append(os.fspath(path))
        return real_scandir(path)

    monkeypatch.setattr(os, "scandir", counting_scandir)

    steady = await mgr._load_session_metas()
    assert [meta.id for meta in steady] == [first.id]
    assert not root_scans

    second = SessionMetadata(name="second")
    _write_metadata(mgr, second)
    grown = await mgr._load_session_metas()
    assert {meta.id for meta in grown} == {first.id, second.id}
    assert len(root_scans) == 1

    assert await mgr._load_session_metas()
    assert len(root_scans) == 1

    shutil.rmtree(mgr._session_dir(second.id))
    shrunk = await mgr._load_session_metas()
    assert [meta.id for meta in shrunk] == [first.id]
    assert len(root_scans) == 2
