from __future__ import annotations

import json
import uuid
from pathlib import Path
from unittest.mock import patch
from urllib.parse import quote

import pytest
from conftest import make_session_mgr as _make_session_mgr
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager


@pytest.mark.asyncio
async def test_save_chat_event_assigns_unique_uuid_ids(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(name="UUID events")
  await mgr.save_metadata(meta)

  first = {"type": "master_done"}
  second = {"type": "master_done"}
  await mgr.save_chat_event(meta.id, first)
  await mgr.save_chat_event(meta.id, second)

  events = mgr.load_chat_events_sync(meta.id)
  assert events[0]["id"] == first["id"]
  assert events[1]["id"] == second["id"]
  assert uuid.UUID(events[0]["id"])
  assert uuid.UUID(events[1]["id"])
  assert events[0]["id"] != events[1]["id"]


@pytest.mark.asyncio
async def test_round_rating_metadata_migration_is_idempotent(tmp_path: Path) -> None:
  seed_mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(
      name="Rated session",
      round_ratings={
          "5": "thumbs_up",
          "event-uuid": "thumbs_down",
      },
  )
  await seed_mgr.save_metadata(meta)

  load_mgr = SessionManager(seed_mgr._cfg)
  real_save = load_mgr.save_metadata
  save_calls = 0

  async def counting_save(updated: SessionMetadata) -> None:
    nonlocal save_calls
    save_calls += 1
    await real_save(updated)

  with patch.object(load_mgr, "save_metadata", side_effect=counting_save):
    first = await load_mgr.get_session(meta.id)
    second = await load_mgr.get_session(meta.id)

  assert first is not None
  assert second is not None
  assert first.round_ratings == {
      "legacy:5": "thumbs_up",
      "event-uuid": "thumbs_down",
  }
  assert second.round_ratings == first.round_ratings
  assert save_calls == 1

  raw = json.loads((seed_mgr._cfg.sessions_dir / meta.id / "metadata.json").read_text(encoding="utf-8"))
  assert raw["round_ratings"] == first.round_ratings


@pytest.mark.asyncio
async def test_rate_round_uses_string_round_id_path_key(tmp_path: Path) -> None:
  mgr = _make_session_mgr(tmp_path)
  meta = SessionMetadata(name="API ratings")
  await mgr.save_metadata(meta)

  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: mgr

  round_id = "legacy:5"
  with TestClient(app) as client:
    response = client.post(
        f"/api/sessions/{meta.id}/rounds/{quote(round_id, safe='')}/rate",
        json={"rating": "thumbs_up"},
    )

  assert response.status_code == 200
  assert response.json()["round_ratings"] == {round_id: "thumbs_up"}
