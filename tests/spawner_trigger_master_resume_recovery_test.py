"""Tests for trigger-master resume recovery behavior."""

from pathlib import Path
from unittest.mock import Mock

import pytest

from src.core.config import CharlieBotConfig
from src.core.master_trigger import is_resume_not_found_error, trigger_master
from src.core.models import BackendOption, SessionCallbacks, SessionMetadata


def _build_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )


class FakeSessionManager:
  """Minimal session manager test double for trigger-master tests."""

  def __init__(self, meta: SessionMetadata | None) -> None:
    self._meta = meta
    self.saved_metas: list[SessionMetadata] = []
    self.persisted_cc_session_ids: list[str] = []

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    return self._meta

  async def resolve_successor_chain(self, session_id: str) -> SessionMetadata | None:
    return self._meta

  async def save_metadata(self, meta: SessionMetadata) -> None:
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

  async def persist_master_run(self, session_id: str, record) -> None:
    if self._meta is not None:
      self._meta.master_run = record

  def callbacks(self) -> SessionCallbacks:
    return SessionCallbacks(
        persist_and_broadcast=self.persist_and_broadcast,
        update_thinking_state=self.update_thinking_state,
        mark_unread=self.mark_unread,
        persist_cc_session_id=self.persist_cc_session_id,
        has_completed_round=self.has_completed_round,
        persist_master_run=self.persist_master_run,
    )


@pytest.mark.asyncio
async def test_stale_resume_id_retries_once_without_resume_and_does_not_persist(monkeypatch: pytest.MonkeyPatch) -> None:
  """Stale resume/session-not-found errors should retry once and recover.

  Persistence of the new cc_session_id is the consumer's job (inside
  run_message), not trigger_master's, so trigger_master must not touch the
  anchor or call persist_cc_session_id.
  """
  cfg = _build_cfg()
  session_id = "session-1"
  meta = SessionMetadata(id=session_id, name="Test Session", cc_session_id="stale-id", backend="codex-o3")
  session_mgr = FakeSessionManager(meta)
  call_resume_ids: list[str | None] = []
  call_backend_options: list[BackendOption] = []
  call_flags: list[tuple[bool, bool]] = []

  async def fake_run_message(*args: object, **kwargs: object) -> str | None:
    call_resume_ids.append(args[1].cc_session_id)
    call_backend_options.append(kwargs["backend_option"])
    call_flags.append((kwargs["skip_user_event"], kwargs["auto_trigger"]))
    if len(call_resume_ids) == 1:
      raise RuntimeError("Codex --resume failed: conversation not found")
    return "fresh-id"

  mock_log = Mock()
  monkeypatch.setattr("src.core.master_trigger.run_message", fake_run_message)
  monkeypatch.setattr("src.core.master_trigger.log", mock_log)

  await trigger_master(session_id, "worker summary", cfg, session_mgr)

  assert call_resume_ids == ["stale-id", None]
  assert [backend_option.id for backend_option in call_backend_options] == ["codex-o3", "codex-o3"]
  assert [backend_option.model for backend_option in call_backend_options] == ["o3", "o3"]
  assert call_backend_options[0] is call_backend_options[1]
  assert call_flags == [(True, True), (True, True)]
  assert session_mgr._meta is not None
  # trigger_master no longer persists the anchor; the consumer owns it.
  assert session_mgr._meta.cc_session_id == "stale-id"
  assert session_mgr.persisted_cc_session_ids == []
  assert session_mgr.saved_metas == []
  assert any(call.args[0] == "trigger_master_invalid_resume_detected" for call in mock_log.warning.call_args_list)
  assert any(call.args[0] == "trigger_master_retry_without_resume" for call in mock_log.info.call_args_list)
  assert any(call.args[0] == "trigger_master_resume_recovery_succeeded" for call in mock_log.info.call_args_list)


@pytest.mark.asyncio
async def test_non_recoverable_error_does_not_retry_and_failure_is_preserved(monkeypatch: pytest.MonkeyPatch) -> None:
  """Non-resume failures should not retry and should remain hard failures."""
  cfg = _build_cfg()
  session_id = "session-2"
  meta = SessionMetadata(id=session_id, name="Test Session", cc_session_id="valid-id", backend="codex-o3")
  session_mgr = FakeSessionManager(meta)
  call_count = 0
  call_backend_options: list[BackendOption] = []

  async def fake_run_message(*args: object, **kwargs: object) -> str | None:
    nonlocal call_count
    call_count += 1
    call_backend_options.append(kwargs["backend_option"])
    raise RuntimeError("backend crashed unexpectedly")

  mock_log = Mock()
  monkeypatch.setattr("src.core.master_trigger.run_message", fake_run_message)
  monkeypatch.setattr("src.core.master_trigger.log", mock_log)

  await trigger_master(session_id, "worker summary", cfg, session_mgr)

  assert call_count == 1
  assert [backend_option.id for backend_option in call_backend_options] == ["codex-o3"]
  assert session_mgr._meta is not None
  assert session_mgr._meta.cc_session_id == "valid-id"
  assert any(call.args[0] == "trigger_master_failed" for call in mock_log.error.call_args_list)
  assert not any(call.args[0] == "trigger_master_invalid_resume_detected" for call in mock_log.warning.call_args_list)


@pytest.mark.asyncio
async def test_valid_resume_path_is_unchanged(monkeypatch: pytest.MonkeyPatch) -> None:
  """Successful run with valid resume ID should stay single-attempt."""
  cfg = _build_cfg()
  session_id = "session-3"
  meta = SessionMetadata(id=session_id, name="Test Session", cc_session_id="valid-id", backend="codex-o3")
  session_mgr = FakeSessionManager(meta)
  call_resume_ids: list[str | None] = []
  call_backend_options: list[BackendOption] = []
  call_flags: list[tuple[bool, bool]] = []

  async def fake_run_message(*args: object, **kwargs: object) -> str | None:
    call_resume_ids.append(args[1].cc_session_id)
    call_backend_options.append(kwargs["backend_option"])
    call_flags.append((kwargs["skip_user_event"], kwargs["auto_trigger"]))
    return "valid-id"

  mock_log = Mock()
  monkeypatch.setattr("src.core.master_trigger.run_message", fake_run_message)
  monkeypatch.setattr("src.core.master_trigger.log", mock_log)

  await trigger_master(session_id, "worker summary", cfg, session_mgr)

  assert call_resume_ids == ["valid-id"]
  assert [backend_option.id for backend_option in call_backend_options] == ["codex-o3"]
  assert call_flags == [(True, True)]
  assert session_mgr._meta is not None
  assert session_mgr._meta.cc_session_id == "valid-id"
  assert not any(call.args[0] == "trigger_master_retry_without_resume" for call in mock_log.info.call_args_list)


@pytest.mark.asyncio
async def test_scheduled_task_auto_trigger_uses_session_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """Scheduled task auto-triggers should keep using the completed worker's session backend."""
  cfg = _build_cfg()
  session_id = "session-4"
  meta = SessionMetadata(
      id=session_id,
      name="Scheduled: nightly",
      backend="claude-opus-4.6",
      scheduled_task="nightly",
  )
  session_mgr = FakeSessionManager(meta)
  call_backend_options: list[BackendOption] = []
  call_session_backends: list[str] = []
  call_summaries: list[str] = []

  async def fake_run_message(*args: object, **kwargs: object) -> str | None:
    call_session_backends.append(args[1].backend)
    call_summaries.append(args[2])
    call_backend_options.append(kwargs["backend_option"])
    return "claude-master-id"

  monkeypatch.setattr("src.core.master_trigger.run_message", fake_run_message)

  await trigger_master(session_id, "worker summary", cfg, session_mgr)

  assert call_session_backends == ["claude-opus-4.6"]
  assert [option.id for option in call_backend_options] == ["claude-opus-4.6"]
  assert [option.model for option in call_backend_options] == ["claude-opus-4-6"]
  assert call_summaries[0].startswith("[Auto-triggered scheduled task result for 'nightly']")
  # trigger_master no longer persists the anchor; the consumer owns it.
  assert session_mgr.persisted_cc_session_ids == []
  assert session_mgr._meta is not None
  assert session_mgr._meta.cc_session_id is None


def test_codex_no_rollout_found_resume_error_is_stale() -> None:
  assert is_resume_not_found_error(RuntimeError("Codex resume failed for thread abc123: no rollout found"))
  assert is_resume_not_found_error(RuntimeError("thread abc123 no rollout found"))
