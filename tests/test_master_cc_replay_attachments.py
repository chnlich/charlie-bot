"""Attachments ride the master run: run_message threads uploaded_files into the
backend call, and restart replay re-attaches them from the persisted user event."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    SESSIONS_SESSION_MANAGER_PATCH_TARGET,
    TerminateFlagBackend,
    backend_option,
    drain_session_consumer,
    mock_session_callbacks,
    patch_instructions_content,
)

from src.agents import master_cc, master_cc_queue, master_cc_state
from src.core import config as core_config
from src.core import models


def _make_cfg(tmp_path: Path) -> core_config.CharlieBotConfig:
  return core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [backend_option(id="fake", label="Fake", type="codex", model="fake-model")]},
  )


class _CapturingBackend(TerminateFlagBackend):
  exit_code = 0
  stderr_text = ""

  def __init__(self) -> None:
    self.calls: list[dict] = []

  async def run(self, prompt: str, cwd: str, env: dict, uploaded_files: list[dict] | None = None):
    self.calls.append({"prompt": prompt, "uploaded_files": uploaded_files})
    if False:
      yield {}  # keeps run() an async generator; the consumer's async-for would TypeError on a coroutine


async def _drive_with_capturing_backend(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    session_id: str,
    drive,
):
  cfg = _make_cfg(tmp_path)
  meta = models.SessionMetadata(id=session_id, name="Attach", backend="fake")
  callbacks = mock_session_callbacks()
  backend = _CapturingBackend()
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda *a, **kw: backend)
  patch_instructions_content(monkeypatch)
  master_cc_state._session_queues.pop(meta.id, None)
  master_cc_state._session_consumers.pop(meta.id, None)
  try:
    with (
        patch.object(master_cc_queue.streaming_manager, "broadcast", new=AsyncMock()),
        patch(SESSIONS_SESSION_MANAGER_PATCH_TARGET) as session_mgr_cls,
    ):
      session_mgr_inst = MagicMock()
      session_mgr_inst._has_running_tasks = AsyncMock(return_value=False)
      session_mgr_cls.return_value = session_mgr_inst
      await drive(cfg, meta, callbacks)
      await drain_session_consumer(meta.id, timeout=5)
  finally:
    master_cc_state._session_queues.pop(meta.id, None)
    master_cc_state._session_consumers.pop(meta.id, None)
  return backend


_FILES = [{"filename": "pic.png", "path": "/uploads/pic.png", "size": 3}]


@pytest.mark.asyncio
async def test_run_message_passes_uploaded_files_to_backend(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A message with attachments enqueues them on the work item, and _run_cc's
  backend.run call receives them."""

  async def drive(cfg, meta, callbacks):
    await master_cc.run_message(cfg, meta, "what is in this picture", callbacks, uploaded_files=_FILES)

  backend = await _drive_with_capturing_backend(tmp_path, monkeypatch, session_id="attach-run", drive=drive)

  assert len(backend.calls) == 1
  assert backend.calls[0]["uploaded_files"] == _FILES
  assert backend.calls[0]["prompt"] == "what is in this picture"


@pytest.mark.asyncio
async def test_run_message_without_attachments_passes_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No attachments: backend.run sees uploaded_files=None, exactly as before."""

  async def drive(cfg, meta, callbacks):
    await master_cc.run_message(cfg, meta, "plain", callbacks)

  backend = await _drive_with_capturing_backend(tmp_path, monkeypatch, session_id="attach-none", drive=drive)

  assert len(backend.calls) == 1
  assert backend.calls[0]["uploaded_files"] is None


@pytest.mark.asyncio
async def test_replay_passes_uploaded_files_from_persisted_event_to_backend(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A replayed user event carrying uploaded_files re-attaches them: replay_user_message
  reads the persisted refs and run_message forwards them to backend.run. The replayed
  content text stays as-is apart from the replay-marker prefix."""

  async def drive(cfg, meta, callbacks):
    user_event = {"id": "u1", "type": "user", "content": "what is in this picture", "uploaded_files": _FILES}
    await master_cc.replay_user_message(cfg, meta, user_event, callbacks)

  backend = await _drive_with_capturing_backend(tmp_path, monkeypatch, session_id="attach-replay", drive=drive)

  assert len(backend.calls) == 1
  assert backend.calls[0]["uploaded_files"] == _FILES
  assert backend.calls[0]["prompt"] == master_cc_queue._REPLAY_MARKER + "\n\n" + "what is in this picture"


@pytest.mark.asyncio
async def test_replay_without_attachments_passes_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:

  async def drive(cfg, meta, callbacks):
    await master_cc.replay_user_message(cfg, meta, {"id": "u1", "type": "user", "content": "plain"}, callbacks)

  backend = await _drive_with_capturing_backend(tmp_path, monkeypatch, session_id="attach-replay-none", drive=drive)

  assert len(backend.calls) == 1
  assert backend.calls[0]["uploaded_files"] is None
