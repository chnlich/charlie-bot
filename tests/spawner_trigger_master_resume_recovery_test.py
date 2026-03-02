"""Tests for trigger-master resume recovery behavior."""

import sys
from pathlib import Path
from typing import Optional
from unittest.mock import Mock

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from src.core.models import SessionMetadata
from src.core.spawner import _trigger_master


class FakeSessionManager:
  """Minimal session manager test double for trigger-master tests."""

  def __init__(self, meta: Optional[SessionMetadata]) -> None:
    self._meta = meta
    self.saved_metas: list[SessionMetadata] = []

  async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
    return self._meta

  async def save_metadata(self, meta: SessionMetadata) -> None:
    self._meta = meta
    self.saved_metas.append(meta.model_copy(deep=True))

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    return None

  async def mark_unread(self, session_id: str) -> None:
    return None


@pytest.mark.asyncio
async def test_stale_resume_id_retries_once_without_resume_and_persists_new_id(monkeypatch: pytest.MonkeyPatch) -> None:
  """Stale resume/session-not-found errors should retry once and recover."""
  session_id = "session-1"
  meta = SessionMetadata(id=session_id, name="Test Session", cc_session_id="stale-id", backend="codex")
  session_mgr = FakeSessionManager(meta)
  call_resume_ids: list[Optional[str]] = []

  async def fake_run_message(*args: object, **kwargs: object) -> Optional[str]:
    call_resume_ids.append(args[1].cc_session_id)
    if len(call_resume_ids) == 1:
      raise RuntimeError("Codex --resume failed: conversation not found")
    return "fresh-id"

  mock_log = Mock()
  monkeypatch.setattr("src.core.spawner.run_message", fake_run_message)
  monkeypatch.setattr("src.core.spawner.log", mock_log)

  await _trigger_master(session_id, "worker summary", object(), session_mgr)

  assert call_resume_ids == ["stale-id", None]
  assert session_mgr._meta is not None
  assert session_mgr._meta.cc_session_id == "fresh-id"
  assert any(saved.cc_session_id == "fresh-id" for saved in session_mgr.saved_metas)
  assert any(call.args[0] == "trigger_master_invalid_resume_detected" for call in mock_log.warning.call_args_list)
  assert any(call.args[0] == "trigger_master_retry_without_resume" for call in mock_log.info.call_args_list)
  assert any(call.args[0] == "trigger_master_resume_recovery_succeeded" for call in mock_log.info.call_args_list)


@pytest.mark.asyncio
async def test_non_recoverable_error_does_not_retry_and_failure_is_preserved(monkeypatch: pytest.MonkeyPatch) -> None:
  """Non-resume failures should not retry and should remain hard failures."""
  session_id = "session-2"
  meta = SessionMetadata(id=session_id, name="Test Session", cc_session_id="valid-id", backend="codex")
  session_mgr = FakeSessionManager(meta)
  call_count = 0

  async def fake_run_message(*args: object, **kwargs: object) -> Optional[str]:
    nonlocal call_count
    call_count += 1
    raise RuntimeError("backend crashed unexpectedly")

  mock_log = Mock()
  monkeypatch.setattr("src.core.spawner.run_message", fake_run_message)
  monkeypatch.setattr("src.core.spawner.log", mock_log)

  await _trigger_master(session_id, "worker summary", object(), session_mgr)

  assert call_count == 1
  assert session_mgr._meta is not None
  assert session_mgr._meta.cc_session_id == "valid-id"
  assert any(call.args[0] == "trigger_master_failed" for call in mock_log.error.call_args_list)
  assert not any(call.args[0] == "trigger_master_invalid_resume_detected" for call in mock_log.warning.call_args_list)


@pytest.mark.asyncio
async def test_valid_resume_path_is_unchanged(monkeypatch: pytest.MonkeyPatch) -> None:
  """Successful run with valid resume ID should stay single-attempt."""
  session_id = "session-3"
  meta = SessionMetadata(id=session_id, name="Test Session", cc_session_id="valid-id", backend="codex")
  session_mgr = FakeSessionManager(meta)
  call_resume_ids: list[Optional[str]] = []

  async def fake_run_message(*args: object, **kwargs: object) -> Optional[str]:
    call_resume_ids.append(args[1].cc_session_id)
    return "valid-id"

  mock_log = Mock()
  monkeypatch.setattr("src.core.spawner.run_message", fake_run_message)
  monkeypatch.setattr("src.core.spawner.log", mock_log)

  await _trigger_master(session_id, "worker summary", object(), session_mgr)

  assert call_resume_ids == ["valid-id"]
  assert session_mgr._meta is not None
  assert session_mgr._meta.cc_session_id == "valid-id"
  assert not any(call.args[0] == "trigger_master_retry_without_resume" for call in mock_log.info.call_args_list)
