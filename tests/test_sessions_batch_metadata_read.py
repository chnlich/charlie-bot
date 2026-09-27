"""Tests for the batched metadata read used by session listings."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import make_session_mgr as _make_session_mgr

from src.core.models import SessionMetadata, SessionStatus
from src.core.sessions import SessionManager


def _write_metadata(mgr: SessionManager, meta: SessionMetadata, raw: str | None = None) -> Path:
  path = mgr._metadata_path(meta.id)
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(meta.model_dump_json() if raw is None else raw, encoding="utf-8")
  return path


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
