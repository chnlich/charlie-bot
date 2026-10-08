"""Tests for the batched metadata read used by session listings."""

from __future__ import annotations

import json
import pathlib

import conftest
import pytest

from src.infra import config as core_config
from src.infra import models
from src.runtime import sessions, spawner_backends


def _write_metadata(mgr: sessions.SessionManager, meta: models.SessionMetadata, raw: str | None = None) -> pathlib.Path:
  path = mgr._metadata_path(meta.id)
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(meta.model_dump_json() if raw is None else raw, encoding="utf-8")
  return path


@pytest.mark.asyncio
async def test_batch_output_matches_sequential_get_session_for_mixed_fixture(tmp_path: pathlib.Path,) -> None:
  mgr = conftest.make_session_mgr(tmp_path)
  active = models.SessionMetadata(name="active")
  archived = models.SessionMetadata(name="archived", status=models.SessionStatus.ARCHIVED)
  legacy = models.SessionMetadata(name="legacy", round_ratings={"9": "thumbs_up"})
  corrupt = models.SessionMetadata(name="corrupt")
  _write_metadata(mgr, active)
  _write_metadata(mgr, archived)
  _write_metadata(mgr, legacy)
  _write_metadata(mgr, corrupt, "{corrupt")

  batch_result = await mgr._load_session_metas()
  sequential_mgr = sessions.SessionManager(mgr._cfg)
  sequential_result: list[models.SessionMetadata] = []
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
async def test_session_pinned_to_a_backend_the_config_no_longer_defines_loads_lists_and_refuses_a_new_run(
    tmp_path: pathlib.Path) -> None:
  """Stored sessions carry removed subscription backend ids; they stay readable and a new run is refused."""
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / "home", backends={"options": [conftest.OPUS_BACKEND_OPTION]})
  mgr = sessions.SessionManager(cfg)
  stored = models.SessionMetadata(name="stored", backend="claude-fable-sub")
  _write_metadata(mgr, stored)
  events_path = mgr.get_chat_events_path(stored.id)
  events_path.parent.mkdir(parents=True, exist_ok=True)
  events_path.write_text(json.dumps(conftest.user_event("hello")) + "\n", encoding="utf-8")

  loaded = await mgr.get_session(stored.id)
  listed = await mgr.list_sessions()

  assert loaded is not None
  assert loaded.backend == "claude-fable-sub"
  assert [meta.id for meta in listed] == [stored.id]
  assert [event["content"] for event in mgr.load_chat_events_sync(stored.id)] == ["hello"]
  with pytest.raises(ValueError, match="refusing to substitute"):
    spawner_backends._resolve_session_default_backend_model(cfg, loaded)
