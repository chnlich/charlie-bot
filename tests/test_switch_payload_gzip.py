"""The switch fetches' body-keyed gzip memo (src/api/sessions.py
_switch_payload_response): the view and bootstrap handlers ship their
compressed form byte-identical to the plain render, re-compress nothing on a
repeat of the same body, and re-compress when the body changes."""

from __future__ import annotations

import gzip
import json
from pathlib import Path
from unittest.mock import patch

import pytest
from conftest import _page_request, make_home_session

from src.api import deps
from src.api.sessions import get_session_bootstrap, get_session_view
from src.core.threads import ThreadManager
from src.core.triggers import TriggerManager


async def _call(handler, session_id, request, meta, mgr, cfg):
  """One direct handler call with the dependency shape each signature carries."""
  if handler is get_session_view:
    return await handler(session_id, request, meta, mgr, ThreadManager(cfg), cfg)
  return await handler(session_id, request, meta, mgr, cfg)


@pytest.fixture(autouse=True)
def _fresh_switch_memo():
  """The module-level memo persists across tests; every test starts empty."""
  from src.api.sessions import _switch_gzip_memo

  _switch_gzip_memo.clear()


@pytest.mark.asyncio
async def test_switch_gzip_ships_precompressed_body(tmp_path: Path) -> None:
  """A gzip-accepting switch fetch serves the memo's gzip form: the decompressed
  bytes equal the plain body, and the vary header names the negotiator."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")
  meta = await mgr.get_session(session.id)
  with patch.object(deps, "_trigger_manager", TriggerManager(cfg, mgr)):
    for handler in (get_session_view, get_session_bootstrap):
      plain = await _call(handler, session.id, _page_request(), meta, mgr, cfg)
      gz = await _call(handler, session.id, _page_request("gzip"), meta, mgr, cfg)
      assert gz.headers["content-encoding"] == "gzip"
      assert gz.headers["vary"] == "Accept-Encoding"
      assert gzip.decompress(gz.body) == plain.body
      assert json.loads(plain.body)["session"]["id"] == session.id


@pytest.mark.asyncio
async def test_switch_gzip_repeat_serves_memo_without_recompress(tmp_path: Path) -> None:
  """A repeat switch fetch of the same body serves the memo's bytes and
  re-compresses nothing."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")
  meta = await mgr.get_session(session.id)
  with patch.object(deps, "_trigger_manager", TriggerManager(cfg, mgr)):
    first = await _call(get_session_view, session.id, _page_request("gzip"), meta, mgr, cfg)

    def explode(data, compresslevel=9, *, mtime=None):
      raise AssertionError("repeat switch fetch re-ran the deflate")

    with patch("src.api.sessions.gzip.compress", explode):
      second = await _call(get_session_view, session.id, _page_request("gzip"), meta, mgr, cfg)
    assert second.body == first.body


@pytest.mark.asyncio
async def test_switch_gzip_changed_body_recompresses(tmp_path: Path) -> None:
  """A renamed session changes the rendered body: the next gzip fetch
  compresses that body once and its decompressed bytes carry the new name."""
  cfg, mgr, session = await make_home_session(tmp_path, name="t")
  meta = await mgr.get_session(session.id)
  with patch.object(deps, "_trigger_manager", TriggerManager(cfg, mgr)):
    await _call(get_session_view, session.id, _page_request("gzip"), meta, mgr, cfg)
    await mgr.rename_session(session.id, "renamed-2")
    meta = await mgr.get_session(session.id)
    gz = await _call(get_session_view, session.id, _page_request("gzip"), meta, mgr, cfg)
    plain = await _call(get_session_view, session.id, _page_request(), meta, mgr, cfg)
    assert gzip.decompress(gz.body) == plain.body
    assert json.loads(plain.body)["session"]["name"] == "renamed-2"


@pytest.mark.asyncio
async def test_switch_plain_request_stays_uncompressed(tmp_path: Path) -> None:
  """A client sending no Accept-Encoding reads the plain render: no
  Content-Encoding header, and the switch memo gains no entry."""
  from src.api.sessions import _switch_gzip_memo

  cfg, mgr, session = await make_home_session(tmp_path, name="t")
  meta = await mgr.get_session(session.id)
  with patch.object(deps, "_trigger_manager", TriggerManager(cfg, mgr)):
    plain = await _call(get_session_bootstrap, session.id, _page_request(), meta, mgr, cfg)
    assert "content-encoding" not in plain.headers
    assert len(_switch_gzip_memo) == 0
