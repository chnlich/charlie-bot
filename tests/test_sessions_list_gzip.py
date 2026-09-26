"""The sidebar's root session list (GET /api/sessions/): the parsed body equals
the response-model render, an unchanged corpus re-renders nothing."""

from __future__ import annotations

import gzip
import json
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import (
    RESPONSES_GZIP_LEVEL1_PATCH_TARGET,
    _page_request,
    assert_gzip_served,
    fresh_state_fixture,
    gzip_explode_compress,
    make_home_session,
)
from fastapi.encoders import jsonable_encoder

from src.api.sessions import _sessions_list_gzip_memo, list_sessions, project_worker_threads
from src.core.models import SessionStatus
from src.core.threads import ThreadManager

_fresh_list_memo = fresh_state_fixture(_sessions_list_gzip_memo.clear)


async def _call(cfg, mgr, thread_mgr, accept_encoding: str = ""):
  return await list_sessions(_page_request(accept_encoding), session_mgr=mgr, cfg=cfg, thread_mgr=thread_mgr)


@pytest.mark.asyncio
async def test_list_ships_precompressed_body(tmp_path: Path) -> None:
  """A gzip-accepting list fetch serves the memo's gzip form: the decompressed
  bytes equal the plain render, and the parsed body equals the response-model
  render of the same rows — the transport change leaves the document as-is."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")
  thread_mgr = ThreadManager(cfg)
  plain = await _call(cfg, mgr, thread_mgr)
  gz = await _call(cfg, mgr, thread_mgr, "gzip")
  assert_gzip_served(gz)
  assert gzip.decompress(gz.body) == plain.body
  # Parity witness: the parsed body equals the stamped-copy path's render.
  sessions = await mgr.list_sessions(
      status=SessionStatus.ACTIVE,
      scheduled=False,
      include_running_status=True,
      include_pending_trigger_status=True,
      include_pending_plan_approval=True)  # the route's own args
  rows = await project_worker_threads(sessions, cfg, thread_mgr)
  assert json.loads(plain.body) == jsonable_encoder(rows)
  assert json.loads(plain.body)[0]["id"] == session.id


@pytest.mark.asyncio
async def test_list_repeat_serves_memo_without_recompress(tmp_path: Path) -> None:
  """A repeat list fetch of an unchanged body serves the memo's bytes and
  re-compresses nothing."""
  cfg, mgr, _session = await make_home_session(tmp_path, name="t")
  thread_mgr = ThreadManager(cfg)
  first = await _call(cfg, mgr, thread_mgr, "gzip")
  with patch(RESPONSES_GZIP_LEVEL1_PATCH_TARGET, gzip_explode_compress("repeat session list re-ran the deflate")):
    second = await _call(cfg, mgr, thread_mgr, "gzip")
  assert second.body == first.body


@pytest.mark.asyncio
async def test_list_repeat_serves_whole_body_without_rerender(tmp_path: Path) -> None:
  """A repeat list fetch of an unchanged corpus re-renders nothing: the row
  refs are identity-stable, so the render runs only when a meta reloads."""
  cfg, mgr, _session = await make_home_session(tmp_path, name="t")
  thread_mgr = ThreadManager(cfg)
  await _call(cfg, mgr, thread_mgr, "gzip")

  from src.api import sessions as sessions_api
  with patch.object(sessions_api, "fast_json_bytes",
                    side_effect=AssertionError("repeat session list re-rendered the body")):
    second = await _call(cfg, mgr, thread_mgr, "gzip")
  assert isinstance(second.body, bytes) and second.body


@pytest.mark.asyncio
async def test_list_rerenders_when_a_meta_reloads(tmp_path: Path) -> None:
  """A renamed session reloads its meta; the next fetch re-renders it."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")
  thread_mgr = ThreadManager(cfg)
  await _call(cfg, mgr, thread_mgr, "gzip")
  await mgr.rename_session(session.id, "renamed-2")
  body = json.loads((await _call(cfg, mgr, thread_mgr)).body)
  assert body[0]["name"] == "renamed-2"


@pytest.mark.asyncio
async def test_list_overlay_matches_the_copy_path_render(tmp_path: Path) -> None:
  """The overlay renders thinking_since in the UtcDatetime scheme (the copy
  path's stamp) and next_trigger_at in the plain-datetime scheme — both byte-
  equal to the copy path's render with the fields set."""
  from datetime import UTC, datetime, timedelta

  from src.core import thinking_state
  from src.core.models import PendingTrigger
  from src.core.triggers import TriggerManager

  cfg, mgr, session = await make_home_session(tmp_path, name="t")
  thread_mgr = ThreadManager(cfg)
  trigger_mgr = TriggerManager(cfg, mgr)
  await trigger_mgr._save_trigger(
      PendingTrigger(session_id=session.id, fire_at=datetime.now(UTC) + timedelta(hours=1), message="wake"))
  thinking_state.mark_busy(session.id)
  plain = await _call(cfg, mgr, thread_mgr)
  body = json.loads(plain.body)
  sessions = await mgr.list_sessions(
      status=SessionStatus.ACTIVE,
      scheduled=False,
      include_running_status=True,
      include_pending_trigger_status=True,
      include_pending_plan_approval=True)
  rows = await project_worker_threads(sessions, cfg, thread_mgr)
  assert body == jsonable_encoder(rows)
  assert body[0]["thinking_since"] == thinking_state.busy_since(session.id).isoformat().replace("+00:00", "Z")
  thinking_state.clear_busy(session.id)
  body = json.loads((await _call(cfg, mgr, thread_mgr)).body)
  assert body[0]["thinking_since"] is None
