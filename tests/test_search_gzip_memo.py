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
    gzip_explode_compress,
    make_home_session,
)

import src.api.sessions as sessions_api


@pytest.fixture(autouse=True)
def _fresh_search_gzip_memo() -> None:
  """The module-level memo persists across tests; every test starts empty."""
  sessions_api._search_gzip_memo.clear()


@pytest.mark.asyncio
async def test_search_gzip_ships_precompressed_body(tmp_path: Path) -> None:
  """A gzip-accepting search serves the memo's gzip form: the decompressed
  bytes equal the plain body, the vary header names the negotiator, and the
  parsed payload keeps the row shape."""
  _cfg, mgr, _session = await make_home_session(tmp_path, name="needle")
  plain = await sessions_api.search_sessions(_page_request(), q="needle", session_mgr=mgr)
  gz = await sessions_api.search_sessions(_page_request("gzip"), q="needle", session_mgr=mgr)

  assert_gzip_served(gz)
  assert gzip.decompress(gz.body) == plain.body
  assert json.loads(plain.body)[0]["name"] == "needle"


@pytest.mark.asyncio
async def test_search_gzip_repeat_serves_memo_without_recompress(tmp_path: Path) -> None:
  """A repeat search of the same body serves the memo's bytes and
  re-compresses nothing."""
  _cfg, mgr, _session = await make_home_session(tmp_path, name="needle")
  first = await sessions_api.search_sessions(_page_request("gzip"), q="needle", session_mgr=mgr)

  with patch(RESPONSES_GZIP_LEVEL1_PATCH_TARGET, gzip_explode_compress("repeat search re-ran the deflate")):
    second = await sessions_api.search_sessions(_page_request("gzip"), q="needle", session_mgr=mgr)
  assert second.body == first.body


@pytest.mark.asyncio
async def test_search_plain_request_stays_uncompressed(tmp_path: Path) -> None:
  """A client sending no Accept-Encoding reads the plain render: no
  Content-Encoding header, and the search gzip memo gains no entry."""
  _cfg, mgr, _session = await make_home_session(tmp_path, name="needle")
  plain = await sessions_api.search_sessions(_page_request(), q="needle", session_mgr=mgr)

  assert "content-encoding" not in plain.headers
  assert len(sessions_api._search_gzip_memo) == 0
