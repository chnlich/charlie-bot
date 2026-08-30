"""Regression tests for /api/internal/delegate takeoff gate behavior."""

from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from conftest import (
  OPUS_BACKEND_ID,
  OPUS_BACKEND_OPTION,
  FakeSessionManager,
  capture_create_logged_task,
)
from conftest import THREE_BACKEND_OPTIONS as VERIFY_BACKEND_OPTIONS
from fastapi import HTTPException

from src.api import internal
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import (
  DelegateRequest,
  SessionMetadata,
  SpawnRequest,
  TaskType,
  ThreadMetadata,
)
from src.core.takeoff_gate import DelegationBlockedError, check_takeoff_gate


def _build_request(
    task_type: TaskType = TaskType.IMPLEMENT,
    repo_path: str | None = "/tmp/repo",
    base_branch: str | None = "main",
    backend: str | None = "codex-o3",
) -> DelegateRequest:
  return DelegateRequest(
      session_id="session-id",
      description="Do work",
      base_branch=base_branch,
      backend=backend,
      repo_path=repo_path,
      task_type=task_type,
  )


def _user_event(content: str, timestamp: str | None = None) -> dict[str, Any]:
  event: dict[str, Any] = {"type": ET.USER, "content": content}
  if timestamp is not None:
    event["timestamp"] = timestamp
  return event


def _scheduled_trigger_event(content: str, timestamp: str | None = None) -> dict[str, Any]:
  event: dict[str, Any] = {"type": ET.SCHEDULED_TRIGGER, "content": content}
  if timestamp is not None:
    event["timestamp"] = timestamp
  return event


def _patch_delegate_spawn_rig(
    monkeypatch: pytest.MonkeyPatch,
    req: DelegateRequest,
    session_mgr: Any,
    thread_mgr: Any,
    captured: dict[str, Any],
) -> None:
  """Install the resolve/spawn/create_logged_task/get_config fakes shared by the delegate_task
  flow tests. The resolve fake is awaited directly, so its body asserts the session and requested
  backend at call time. The spawn fake is never awaited — create_logged_task's capture stub closes
  the coroutine — so the capture (not this body) is what pins its bound arguments for assertions."""
  async def fake_resolve_requested_subagent_backend_model(
      session_id: str,
      cfg: Any,
      mgr: Any,
      requested_backend: str | None = None,
  ) -> tuple[str, str]:
    assert session_id == req.session_id
    assert mgr is session_mgr
    assert requested_backend == "codex-o3"
    return "codex-o3", "o3"

  async def fake_spawn_worker(
      session_id: str,
      description: str,
      thread_id: str,
      cfg: Any,
      mgr: Any,
      t_mgr: Any,
      request: SpawnRequest | None = None,
  ) -> None:
    return None

  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(internal, "create_logged_task", capture_create_logged_task(captured))
  monkeypatch.setattr(internal, "get_config", lambda: object())


def test_takeoff_gate_blocks_takeoff_followed_by_ordinary_user_message() -> None:
  session_mgr = FakeSessionManager([
      _user_event("Take Off"),
      _user_event("One more ordinary message"),
  ])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_allows_takeoff_followed_by_trigger_user_message() -> None:
  session_mgr = FakeSessionManager([
      _user_event("take off"),
      _scheduled_trigger_event("[Scheduled trigger fired] training completed"),
  ])

  check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_scheduled_trigger_does_not_mint_takeoff() -> None:
  session_mgr = FakeSessionManager([_scheduled_trigger_event("[Scheduled trigger fired] take off")])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_scheduled_trigger_excluded_by_type_regardless_of_content() -> None:
  """An ET.SCHEDULED_TRIGGER event is excluded by event type, not by content prefix."""
  session_mgr = FakeSessionManager([_scheduled_trigger_event("take off")])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_real_user_literal_banner_now_mints_takeoff() -> None:
  """Forward fix: a real human typing the literal banner text now mints a takeoff window.

  Previously the prefix check silently ignored a real ET.USER message whose content
  started with the scheduled-trigger banner; the type-based gate no longer does.
  """
  session_mgr = FakeSessionManager([_user_event("[Scheduled trigger fired] take off")])

  check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_nested_tool_result_does_not_mint_takeoff() -> None:
  session_mgr = FakeSessionManager([{
      "type": ET.USER,
      "message": {
          "role": "user",
          "content": [{"type": ET.TOOL_RESULT, "content": "take off"}],
      },
  }])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_nested_tool_result_does_not_cancel_ordinary_takeoff() -> None:
  session_mgr = FakeSessionManager([
      _user_event("take off"),
      {
          "type": ET.USER,
          "message": {
              "role": "user",
              "content": [{"type": ET.TOOL_RESULT, "content": "not a command"}],
          },
      },
  ])

  check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_ignores_task_delegated_metadata() -> None:
  session_mgr = FakeSessionManager([{
      "type": ET.TASK_DELEGATED,
      "description": "take off",
      "delegate_invocation": {"task_type": "implement"},
  }])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_allows_repeated_ordinary_takeoff_after_task_delegated_event() -> None:
  session_mgr = FakeSessionManager([
      _user_event("take off"),
      {
          "type": ET.TASK_DELEGATED,
          "thread_id": "thread-id",
          "description": "Do work",
          "timestamp": "2026-07-18T12:00:00+00:00",
          "backend": "codex-o3",
          "model": "o3",
          "delegate_invocation": {
              "task_type": "implement",
              "repo_path": "/tmp/repo",
              "base_branch": "main",
              "task_spec_file": None,
              "reviewer_context_file": None,
              "keep_worktree": False,
              "backend": "codex-o3",
          },
      },
  ])

  check_takeoff_gate("session-id", session_mgr)
  check_takeoff_gate("session-id", session_mgr)


def test_pre_takeoff_window_survives_real_user_messages_and_expires_at_12_hours() -> None:
  issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  session_mgr = FakeSessionManager([
      _user_event("PRE\n\t TAKE   OFF", issued_at.isoformat()),
      _user_event("A later real user message"),
  ])

  check_takeoff_gate(
      "session-id",
      session_mgr,
      now=issued_at + timedelta(hours=12) - timedelta(microseconds=1),
  )
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr, now=issued_at + timedelta(hours=12))


def test_new_pre_takeoff_starts_a_new_window() -> None:
  first_issued_at = datetime(2026, 7, 17, 12, 0, tzinfo=UTC)
  second_issued_at = datetime(2026, 7, 18, 12, 0, tzinfo=UTC)
  session_mgr = FakeSessionManager([
      _user_event("pre take off", first_issued_at.isoformat()),
      _user_event("normal follow-up"),
      _user_event("pre take off", second_issued_at.isoformat()),
      _user_event("another normal follow-up"),
  ])

  check_takeoff_gate("session-id", session_mgr, now=second_issued_at + timedelta(hours=11))


def test_ordinary_takeoff_matching_is_independent_from_pre_matching() -> None:
  session_mgr = FakeSessionManager([_user_event("pre\n take\t off")])

  check_takeoff_gate("session-id", session_mgr)


def test_ordinary_takeoff_needs_no_timestamp_and_is_not_expiring() -> None:
  session_mgr = FakeSessionManager([_user_event("take\n off")])

  check_takeoff_gate("session-id", session_mgr, now=datetime(2099, 1, 1, tzinfo=UTC))


@pytest.mark.parametrize("timestamp", [None, "not-a-timestamp"])
def test_pre_takeoff_with_missing_or_unparseable_timestamp_fails_closed(timestamp: str | None) -> None:
  session_mgr = FakeSessionManager([
      _user_event("pre take off", timestamp),
      _user_event("a later real user message"),
  ])

  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", session_mgr)


def test_takeoff_gate_blocks_when_no_user_string_message_contains_takeoff() -> None:
  session_mgr = FakeSessionManager([
      {"type": ET.USER, "content": "please proceed"},
      {"type": ET.USER, "content": [{"type": ET.TOOL_RESULT, "content": "take off"}]},
  ])

  with pytest.raises(DelegationBlockedError) as exc_info:
    check_takeoff_gate("session-id", session_mgr)

  assert str(exc_info.value) == (
      'Delegation blocked: no active authorization. A valid "pre take off" within 12 hours or '
      '"take off" in the latest real user message is required before delegating.')


@pytest.mark.parametrize("events", [
    [],
    [{"type": ET.ASSISTANT, "content": "take off"}],
])
def test_takeoff_gate_blocks_with_empty_history_or_no_user_messages(events: list[dict[str, Any]]) -> None:
  with pytest.raises(DelegationBlockedError):
    check_takeoff_gate("session-id", FakeSessionManager(events))


@pytest.mark.asyncio
async def test_delegate_task_returns_403_when_takeoff_gate_blocks(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()

  def fake_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr
    raise DelegationBlockedError("blocked")

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 403
  assert exc_info.value.detail == "blocked"
  thread_mgr.create_thread.assert_not_awaited()
  session_mgr.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_delegate_task_repo_task_types_block_without_takeoff(task_type: TaskType) -> None:
  req = _build_request(task_type=task_type)
  session_mgr = FakeSessionManager([{ "type": ET.USER, "content": "please proceed" }])
  thread_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 403
  assert "no active authorization" in exc_info.value.detail
  thread_mgr.create_thread.assert_not_awaited()
  session_mgr.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
async def test_improve_stays_blocked_without_takeoff() -> None:
  req = internal.ImproveRequest(
      session_id="session-id",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      goal="Improve this",
  )
  session_mgr = FakeSessionManager([{ "type": ET.USER, "content": "please proceed" }])
  thread_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 403
  assert "no active authorization" in exc_info.value.detail


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_all_nonverify_delegate_types_can_reuse_ordinary_takeoff(
    task_type: TaskType,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  req = _build_request(task_type=task_type)
  session_mgr = FakeSessionManager([_user_event("take off")])
  cfg = object()

  async def fake_resolve(*args: Any, **kwargs: Any) -> tuple[str, str]:
    del args, kwargs
    return "codex-o3", "o3"

  monkeypatch.setattr(internal, "get_config", lambda: cfg)
  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve)

  for _ in range(3):
    _meta, resolved_cfg, resolved_backend, resolved_model = await internal._authorize_spawn_request(req, session_mgr)
    assert resolved_cfg is cfg
    assert (resolved_backend, resolved_model) == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_improve_uses_the_same_pre_takeoff_gate(monkeypatch: pytest.MonkeyPatch) -> None:
  issued_at = datetime.now(UTC) - timedelta(hours=1)
  req = internal.ImproveRequest(
      session_id="session-id",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      goal="Improve this",
  )
  session_mgr = FakeSessionManager([
      _user_event("pre take off", issued_at.isoformat()),
      _user_event("continue with the approved work"),
  ])
  cfg = object()

  async def fake_resolve(*args: Any, **kwargs: Any) -> tuple[str, str]:
    del args, kwargs
    return "codex-o3", "o3"

  monkeypatch.setattr(internal, "get_config", lambda: cfg)
  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve)
  _meta, resolved_cfg, resolved_backend, resolved_model = await internal._authorize_spawn_request(req, session_mgr)

  assert resolved_cfg is cfg
  assert (resolved_backend, resolved_model) == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_delegate_task_verify_skips_takeoff_gate_and_spawns_repoless(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request(task_type=TaskType.VERIFY, repo_path=None, base_branch=None)
  session_mgr = FakeSessionManager([{ "type": ET.USER, "content": "please proceed" }])
  thread_mgr = AsyncMock()
  thread_mgr.create_thread.return_value = ThreadMetadata(
      id="thread-id",
      session_id=req.session_id,
      description=req.description,
  )
  captured: dict[str, Any] = {}

  def fail_if_gate_runs(session_id: str) -> list[dict[str, Any]]:
    raise AssertionError(f"takeoff gate should not run for verify: {session_id}")

  session_mgr.load_chat_events_sync = fail_if_gate_runs  # type: ignore[method-assign]
  _patch_delegate_spawn_rig(monkeypatch, req, session_mgr, thread_mgr, captured)

  result = await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result == {"thread_id": "thread-id", "description": req.description}
  assert thread_mgr.create_thread.call_args.kwargs["require_review"] is False
  assert captured["request"] == SpawnRequest(
      repo_path=None,
      base_branch=None,
      context=req.context,
      resolved_backend="codex-o3",
      resolved_model="o3",
      task_type=TaskType.VERIFY,
  )
  session_mgr.persist_and_broadcast.assert_awaited_once()
  task_event = session_mgr.persist_and_broadcast.await_args.args[1]
  assert task_event["type"] == ET.TASK_DELEGATED
  assert task_event["thread_id"] == "thread-id"
  assert task_event["description"] == req.description
  assert task_event["backend"] == "codex-o3"
  assert task_event["model"] == "o3"
  assert task_event["delegate_invocation"] == {
      "task_type": "verify",
      "repo_path": None,
      "base_branch": None,
      "task_spec_file": None,
      "reviewer_context_file": None,
      "keep_worktree": False,
      "backend": "codex-o3",
  }


@pytest.mark.asyncio
async def test_delegate_task_verify_rejects_repo_path() -> None:
  req = _build_request(task_type=TaskType.VERIFY, repo_path="/tmp/repo", base_branch=None)
  session_mgr = AsyncMock()
  thread_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "verify delegations are repo-less; omit repo_path"
  session_mgr.get_session.assert_not_awaited()
  thread_mgr.create_thread.assert_not_awaited()


@pytest.mark.asyncio
async def test_delegate_task_does_not_pass_takeoff_gate_to_spawn_worker(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()
  thread_mgr.create_thread.return_value = ThreadMetadata(
      id="thread-id",
      session_id=req.session_id,
      description=req.description,
  )

  captured: dict[str, Any] = {}

  def fake_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)
  _patch_delegate_spawn_rig(monkeypatch, req, session_mgr, thread_mgr, captured)

  result = await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result == {"thread_id": "thread-id", "description": req.description}
  assert captured["session_id"] == req.session_id
  assert captured["description"] == req.description
  assert captured["thread_id"] == "thread-id"
  assert captured["request"] == SpawnRequest(
      repo_path=req.repo_path,
      base_branch=req.base_branch,
      context=req.context,
      resolved_backend="codex-o3",
      resolved_model="o3",
      task_type=TaskType.IMPLEMENT,
  )
  assert not hasattr(captured["request"], "require_takeoff")
  session_mgr.persist_and_broadcast.assert_awaited_once()
  task_event = session_mgr.persist_and_broadcast.await_args.args[1]
  assert task_event["type"] == ET.TASK_DELEGATED
  assert task_event["thread_id"] == "thread-id"
  assert task_event["description"] == req.description
  assert task_event["backend"] == "codex-o3"
  assert task_event["model"] == "o3"
  assert task_event["delegate_invocation"] == {
      "task_type": "implement",
      "repo_path": "/tmp/repo",
      "base_branch": "main",
      "task_spec_file": None,
      "reviewer_context_file": None,
      "keep_worktree": False,
      "backend": "codex-o3",
  }


@pytest.mark.asyncio
async def test_delegate_task_returns_400_for_invalid_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()

  def fake_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr

  async def fake_resolve_requested_subagent_backend_model(*args: Any, **kwargs: Any) -> tuple[str, str]:
    raise ValueError("requested backend 'codex-o3' is not in backend_options")

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)
  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "get_config", lambda: object())

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'codex-o3' is not in backend_options"
  thread_mgr.create_thread.assert_not_awaited()


# --- verify default backend via model_preference ---


def _build_verify_cfg(model_preference: list[str]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=VERIFY_BACKEND_OPTIONS,
      model_preference=model_preference,
  )


class BackendFakeSessionManager:

  def __init__(self, backend: str) -> None:
    self.backend = backend

  async def get_session(self, session_id: str) -> SessionMetadata:
    return SessionMetadata(id=session_id, name="Test", backend=self.backend)


async def _authorize_verify(
    monkeypatch: pytest.MonkeyPatch,
    session_backend: str,
    model_preference: list[str],
    backend: str | None = None,
) -> tuple[str | None, str | None]:
  req = _build_request(task_type=TaskType.VERIFY, repo_path=None, base_branch=None, backend=backend)
  monkeypatch.setattr(internal, "get_config", lambda: _build_verify_cfg(model_preference))
  session_mgr = BackendFakeSessionManager(session_backend)
  _meta, _cfg, resolved_backend, resolved_model = await internal._authorize_spawn_request(req, session_mgr)
  return resolved_backend, resolved_model


@pytest.mark.asyncio
async def test_verify_no_backend_defaults_to_first_differing_preference(monkeypatch: pytest.MonkeyPatch) -> None:
  """Session backend is the first preference entry -> the second (first differing) entry wins."""
  resolved = await _authorize_verify(
      monkeypatch, session_backend=OPUS_BACKEND_ID, model_preference=[OPUS_BACKEND_ID, "codex-o3"])
  assert resolved == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_verify_no_backend_session_backend_not_in_preference_uses_first_entry(
    monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(
      monkeypatch, session_backend="kimi-k2.5", model_preference=[OPUS_BACKEND_ID, "codex-o3"])
  assert resolved == (OPUS_BACKEND_ID, OPUS_BACKEND_OPTION.model)


@pytest.mark.asyncio
async def test_verify_no_backend_empty_preference_keeps_session_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(monkeypatch, session_backend="codex-o3", model_preference=[])
  assert resolved == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_verify_explicit_backend_wins_over_preference(monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(
      monkeypatch,
      session_backend=OPUS_BACKEND_ID,
      model_preference=[OPUS_BACKEND_ID, "codex-o3"],
      backend="kimi-k2.5",
  )
  assert resolved == ("kimi-k2.5", "kimi-k2.5")


@pytest.mark.asyncio
async def test_verify_unknown_explicit_backend_returns_400(monkeypatch: pytest.MonkeyPatch) -> None:
  with pytest.raises(HTTPException) as exc_info:
    await _authorize_verify(
        monkeypatch,
        session_backend=OPUS_BACKEND_ID,
        model_preference=[OPUS_BACKEND_ID, "codex-o3"],
        backend="nonexistent",
    )

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'nonexistent' is not in backend_options"
