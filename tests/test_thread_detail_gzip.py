"""The thread-detail full row's body-keyed gzip memo (served through
src/api/responses.py gzip_body_response): the served gzip form is byte-identical to the plain
render, a repeat of the same body re-compresses nothing, a changed body
re-compresses, the no-Accept-Encoding shape stays uncompressed, and the attach
mode keeps its bodyless slim pair."""

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
)

from src.api.threads import _detail_gzip_memo, get_thread
from src.core.config import CharlieBotConfig
from src.core.models import ThreadMetadata
from src.core.threads import ThreadManager

_OPUS_BACKEND_OPTION = {
    "id": "claude-opus",
    "label": "Claude",
    "type": "cc-claude",
    "model": "claude-opus-4-8",
}


async def _saved_thread(tmp_path: Path) -> tuple[CharlieBotConfig, ThreadManager, ThreadMetadata, Path]:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [_OPUS_BACKEND_OPTION]})
  thread_mgr = ThreadManager(cfg)
  worktree = tmp_path / "worktree"
  worktree.mkdir()
  thread = ThreadMetadata(
      session_id="detail-gzip-session",
      description="detail gzip",
      backend="claude-opus",
      worktree_path=str(worktree),
      claude_session_id="session-123",
  )
  await thread_mgr.save_metadata(thread)
  return cfg, thread_mgr, thread, worktree


_fresh_detail_memo = fresh_state_fixture(_detail_gzip_memo.clear)


@pytest.mark.asyncio
async def test_detail_gzip_ships_precompressed_body(tmp_path: Path) -> None:
  """A gzip-accepting detail fetch serves the memo's gzip form: the
  decompressed bytes equal the plain body and the parsed payload keeps its
  shape (attach pair present, context absent)."""
  cfg, thread_mgr, thread, worktree = await _saved_thread(tmp_path)
  plain = await get_thread(thread.session_id, thread.id, _page_request(), thread_mgr, cfg, attach=False)
  gz = await get_thread(thread.session_id, thread.id, _page_request("gzip"), thread_mgr, cfg, attach=False)
  assert_gzip_served(gz)
  assert gzip.decompress(gz.body) == plain.body
  data = json.loads(plain.body)
  assert data["attach_command"] == f"cd {worktree} && claude --resume session-123"
  assert data["attach_available"] is True
  assert "context" not in data


@pytest.mark.asyncio
async def test_detail_gzip_repeat_serves_memo_without_recompress(tmp_path: Path) -> None:
  """A repeat detail fetch of the same body serves the memo's bytes and
  re-compresses nothing."""
  cfg, thread_mgr, thread, _ = await _saved_thread(tmp_path)
  first = await get_thread(thread.session_id, thread.id, _page_request("gzip"), thread_mgr, cfg, attach=False)

  with patch(RESPONSES_GZIP_LEVEL1_PATCH_TARGET, gzip_explode_compress("repeat detail fetch re-ran the deflate")):
    second = await get_thread(thread.session_id, thread.id, _page_request("gzip"), thread_mgr, cfg, attach=False)
  assert second.body == first.body


@pytest.mark.asyncio
async def test_detail_gzip_changed_body_recompresses(tmp_path: Path) -> None:
  """A metadata rewrite changes the rendered body: the next gzip fetch
  compresses that body once and its decompressed bytes carry the change."""
  cfg, thread_mgr, thread, _ = await _saved_thread(tmp_path)
  await get_thread(thread.session_id, thread.id, _page_request("gzip"), thread_mgr, cfg, attach=False)
  thread.description = "changed"
  await thread_mgr.save_metadata(thread)
  gz = await get_thread(thread.session_id, thread.id, _page_request("gzip"), thread_mgr, cfg, attach=False)
  plain = await get_thread(thread.session_id, thread.id, _page_request(), thread_mgr, cfg, attach=False)
  assert gzip.decompress(gz.body) == plain.body
  assert json.loads(plain.body)["description"] == "changed"


@pytest.mark.asyncio
async def test_detail_plain_request_stays_uncompressed(tmp_path: Path) -> None:
  """A client sending no Accept-Encoding reads the plain render: no
  Content-Encoding header, and the detail memo gains no entry."""
  cfg, thread_mgr, thread, _ = await _saved_thread(tmp_path)
  plain = await get_thread(thread.session_id, thread.id, _page_request(), thread_mgr, cfg, attach=False)
  assert "content-encoding" not in plain.headers
  assert len(_detail_gzip_memo) == 0


@pytest.mark.asyncio
async def test_detail_attach_mode_stays_slim_uncompressed(tmp_path: Path) -> None:
  """The 5 s attach poll's shape is untouched: the pair only, no
  Content-Encoding, and no memo entry (the slim body never enters the gzip
  memo)."""
  cfg, thread_mgr, thread, _ = await _saved_thread(tmp_path)
  attach = await get_thread(thread.session_id, thread.id, _page_request("gzip"), thread_mgr, cfg, attach=True)
  assert set(json.loads(attach.body)) == {"attach_command", "attach_available"}
  assert "content-encoding" not in attach.headers
  assert len(_detail_gzip_memo) == 0
