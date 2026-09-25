"""The capped search's body-keyed gzip memo (src/api/sessions.py search_sessions):
the sidebar search ships its compressed form byte-identical to the plain
render, re-compresses nothing on a repeat of the same body, and a plain
request reads the render with no memo entry."""

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

import src.api.sessions as sessions_api
from src.core.threads import ThreadManager

_fresh_search_gzip_memo = fresh_state_fixture(sessions_api._search_gzip_memo.clear)


@pytest.mark.asyncio
async def test_search_gzip_ships_precompressed_body(tmp_path: Path) -> None:
  """A gzip-accepting search serves the memo's gzip form: the decompressed
  bytes equal the plain body, the vary header names the negotiator, and the
  parsed payload keeps the row shape."""
  cfg, mgr, _session = await make_home_session(tmp_path, name="needle")
  thread_mgr = ThreadManager(cfg)
  plain = await sessions_api.search_sessions(_page_request(), q="needle", session_mgr=mgr, cfg=cfg, thread_mgr=thread_mgr)
  gz = await sessions_api.search_sessions(_page_request("gzip"), q="needle", session_mgr=mgr, cfg=cfg, thread_mgr=thread_mgr)

  assert_gzip_served(gz)
  assert gzip.decompress(gz.body) == plain.body
  assert json.loads(plain.body)[0]["name"] == "needle"


@pytest.mark.asyncio
async def test_search_gzip_repeat_serves_memo_without_recompress(tmp_path: Path) -> None:
  """A repeat search of the same body serves the memo's bytes and
  re-compresses nothing."""
  cfg, mgr, _session = await make_home_session(tmp_path, name="needle")
  thread_mgr = ThreadManager(cfg)
  first = await sessions_api.search_sessions(_page_request("gzip"), q="needle", session_mgr=mgr, cfg=cfg, thread_mgr=thread_mgr)

  with patch(RESPONSES_GZIP_LEVEL1_PATCH_TARGET, gzip_explode_compress("repeat search re-ran the deflate")):
    second = await sessions_api.search_sessions(_page_request("gzip"), q="needle", session_mgr=mgr, cfg=cfg, thread_mgr=thread_mgr)
  assert second.body == first.body


@pytest.mark.asyncio
async def test_search_plain_request_stays_uncompressed(tmp_path: Path) -> None:
  """A client sending no Accept-Encoding reads the plain render: no
  Content-Encoding header, and the search gzip memo gains no entry."""
  cfg, mgr, _session = await make_home_session(tmp_path, name="needle")
  thread_mgr = ThreadManager(cfg)
  plain = await sessions_api.search_sessions(_page_request(), q="needle", session_mgr=mgr, cfg=cfg, thread_mgr=thread_mgr)

  assert "content-encoding" not in plain.headers
  assert len(sessions_api._search_gzip_memo) == 0
