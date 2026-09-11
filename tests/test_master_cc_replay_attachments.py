"""Attachments ride the master run: run_message threads uploaded_files into the
backend call, and restart replay re-attaches them from the persisted user event."""

from __future__ import annotations

from pathlib import Path

import pytest
from conftest import CapturingBackend, run_captured_round

from src.agents import master_cc, master_cc_queue

_FILES = [{"filename": "pic.png", "path": "/uploads/pic.png", "size": 3}]


@pytest.mark.asyncio
async def test_run_message_passes_uploaded_files_to_backend(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A message with attachments enqueues them on the work item, and _run_cc's
  backend.run call receives them."""

  async def drive(cfg, meta, callbacks):
    await master_cc.run_message(cfg, meta, "what is in this picture", callbacks, uploaded_files=_FILES)

  backend = CapturingBackend()
  await run_captured_round(tmp_path, monkeypatch, session_id="attach-run", name="Attach", backend=backend, drive=drive)

  assert len(backend.calls) == 1
  assert backend.calls[0]["uploaded_files"] == _FILES
  assert backend.calls[0]["prompt"] == "what is in this picture"


@pytest.mark.asyncio
async def test_run_message_without_attachments_passes_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No attachments: backend.run sees uploaded_files=None, exactly as before."""

  async def drive(cfg, meta, callbacks):
    await master_cc.run_message(cfg, meta, "plain", callbacks)

  backend = CapturingBackend()
  await run_captured_round(tmp_path, monkeypatch, session_id="attach-none", name="Attach", backend=backend, drive=drive)

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

  backend = CapturingBackend()
  await run_captured_round(
      tmp_path, monkeypatch, session_id="attach-replay", name="Attach", backend=backend, drive=drive)

  assert len(backend.calls) == 1
  assert backend.calls[0]["uploaded_files"] == _FILES
  assert backend.calls[0]["prompt"] == master_cc_queue._REPLAY_MARKER + "\n\n" + "what is in this picture"


@pytest.mark.asyncio
async def test_replay_without_attachments_passes_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:

  async def drive(cfg, meta, callbacks):
    await master_cc.replay_user_message(cfg, meta, {"id": "u1", "type": "user", "content": "plain"}, callbacks)

  backend = CapturingBackend()
  await run_captured_round(
      tmp_path, monkeypatch, session_id="attach-replay-none", name="Attach", backend=backend, drive=drive)

  assert len(backend.calls) == 1
  assert backend.calls[0]["uploaded_files"] is None
