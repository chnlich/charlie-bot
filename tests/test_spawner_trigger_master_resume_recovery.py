"""Tests for trigger-master resume recovery behavior."""

import pathlib
from unittest import mock

import conftest
import pytest

from src.core import config, master_trigger, models
from src.core import event_types as ET

_LOG_PATCH_TARGET = "src.core.master_trigger.log"


def _build_cfg() -> config.CharlieBotConfig:
  return config.CharlieBotConfig(
      charliebot_home=pathlib.Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={"options": [
          conftest.OPUS_BACKEND_OPTION,
          conftest.CODEX_BACKEND_OPTION,
      ]},
  )


class FakeSessionManager:
  """Minimal session manager test double for trigger-master tests."""

  def __init__(self, meta: models.SessionMetadata | None) -> None:
    self._meta = meta
    self.saved_metas: list[models.SessionMetadata] = []
    self.persisted_cc_session_ids: list[str] = []

  async def get_session(self, session_id: str) -> models.SessionMetadata | None:
    return self._meta

  async def resolve_successor_chain(self, session_id: str) -> models.SessionMetadata | None:
    return self._meta

  async def save_metadata(self, meta: models.SessionMetadata) -> None:
    self._meta = meta
    self.saved_metas.append(meta.model_copy(deep=True))

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    return None

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    return None

  async def update_thinking_state(self, session_id: str, *args: object, **kwargs: object) -> None:
    return None

  async def persist_cc_session_id(self, session_id: str, cc_session_id: str) -> str | None:
    if self._meta is not None:
      self._meta.cc_session_id = cc_session_id
    self.persisted_cc_session_ids.append(cc_session_id)
    return cc_session_id

  async def has_completed_round(self, session_id: str) -> bool:
    return False

  async def mark_unread(self, session_id: str) -> None:
    return None

  async def persist_master_run(self, session_id: str, record: models.MasterRunRecord | None) -> None:
    if self._meta is not None:
      self._meta.master_run = record

  def callbacks(self) -> models.SessionCallbacks:
    return models.SessionCallbacks(
        persist_and_broadcast=self.persist_and_broadcast,
        update_thinking_state=self.update_thinking_state,
        mark_unread=self.mark_unread,
        persist_cc_session_id=self.persist_cc_session_id,
        has_completed_round=self.has_completed_round,
        persist_master_run=self.persist_master_run,
    )


@pytest.mark.asyncio
async def test_stale_resume_id_retries_once_without_resume_and_does_not_persist(
    monkeypatch: pytest.MonkeyPatch) -> None:
  """Stale resume/session-not-found errors should retry once and recover.

  Persistence of the new cc_session_id is the consumer's job (inside
  run_message), not trigger_master's, so trigger_master must not touch the
  anchor or call persist_cc_session_id.
  """
  cfg = _build_cfg()
  session_id = "session-1"
  meta = models.SessionMetadata(id=session_id, name="Test Session", cc_session_id="stale-id", backend="codex-o3")
  session_mgr = FakeSessionManager(meta)
  call_resume_ids: list[str | None] = []
  call_backend_options: list[models.BackendOption] = []
  call_flags: list[tuple[bool, bool]] = []

  async def fake_run_message(*args: object, **kwargs: object) -> str | None:
    call_resume_ids.append(args[1].cc_session_id)
    call_backend_options.append(kwargs["backend_option"])
    call_flags.append((kwargs["skip_user_event"], kwargs["auto_trigger"]))
    if len(call_resume_ids) == 1:
      raise RuntimeError("Codex --resume failed: conversation not found")
    return "fresh-id"

  mock_log = mock.Mock()
  monkeypatch.setattr(conftest.MASTER_TRIGGER_RUN_MESSAGE_PATCH_TARGET, fake_run_message)
  monkeypatch.setattr(_LOG_PATCH_TARGET, mock_log)

  await master_trigger.trigger_master(session_id, "worker summary", cfg, session_mgr, ET.CHILD_REPORT)

  assert call_resume_ids == ["stale-id", None]
  assert [backend_option.id for backend_option in call_backend_options] == ["codex-o3", "codex-o3"]
  assert [backend_option.model for backend_option in call_backend_options] == ["o3", "o3"]
  assert call_backend_options[0] is call_backend_options[1]
  assert call_flags == [(True, True), (True, True)]
  assert session_mgr._meta is not None
  assert session_mgr._meta.cc_session_id == "stale-id"
  assert not session_mgr.persisted_cc_session_ids
  assert not session_mgr.saved_metas
  assert any(call.args[0] == "trigger_master_invalid_resume_detected" for call in mock_log.warning.call_args_list)
  assert any(call.args[0] == "trigger_master_retry_without_resume" for call in mock_log.info.call_args_list)
  assert any(call.args[0] == "trigger_master_resume_recovery_succeeded" for call in mock_log.info.call_args_list)


@pytest.mark.asyncio
async def test_non_recoverable_error_does_not_retry_and_failure_is_preserved(monkeypatch: pytest.MonkeyPatch) -> None:
  """Non-resume failures should not retry and should remain hard failures."""
  cfg = _build_cfg()
  session_id = "session-2"
  meta = models.SessionMetadata(id=session_id, name="Test Session", cc_session_id="valid-id", backend="codex-o3")
  session_mgr = FakeSessionManager(meta)
  call_count = 0
  call_backend_options: list[models.BackendOption] = []

  async def fake_run_message(*args: object, **kwargs: object) -> str | None:
    nonlocal call_count
    call_count += 1
    call_backend_options.append(kwargs["backend_option"])
    raise RuntimeError("backend crashed unexpectedly")

  mock_log = mock.Mock()
  monkeypatch.setattr(conftest.MASTER_TRIGGER_RUN_MESSAGE_PATCH_TARGET, fake_run_message)
  monkeypatch.setattr(_LOG_PATCH_TARGET, mock_log)

  await master_trigger.trigger_master(session_id, "worker summary", cfg, session_mgr, ET.CHILD_REPORT)

  assert call_count == 1
  assert [backend_option.id for backend_option in call_backend_options] == ["codex-o3"]
  assert session_mgr._meta is not None
  assert session_mgr._meta.cc_session_id == "valid-id"
  assert any(call.args[0] == "trigger_master_failed" for call in mock_log.error.call_args_list)
  assert not any(call.args[0] == "trigger_master_invalid_resume_detected" for call in mock_log.warning.call_args_list)
