"""Regression tests for /api/internal/delegate takeoff gate behavior."""

from typing import Any, Optional
from unittest.mock import AsyncMock

import pytest
from fastapi import HTTPException

from src.api import internal
from src.core import event_types as ET
from src.core.models import DelegateRequest, SessionMetadata, SpawnRequest, TaskType, ThreadMetadata
from src.core.spawner import DelegationBlockedError


def _build_request(
    task_type: TaskType = TaskType.IMPLEMENT,
    repo_path: Optional[str] = "/tmp/repo",
    base_branch: Optional[str] = "main",
) -> DelegateRequest:
  return DelegateRequest(
      session_id="session-id",
      description="Do work",
      base_branch=base_branch,
      backend="codex-o3",
      repo_path=repo_path,
      task_type=task_type,
  )


class FakeSessionManager:

  def __init__(self, events: list[dict[str, Any]]) -> None:
    self.events = events
    self.persist_and_broadcast = AsyncMock()

  async def get_session(self, session_id: str) -> SessionMetadata:
    return SessionMetadata(id=session_id, name="Test")

  def load_chat_events_sync(self, session_id: str) -> list[dict[str, Any]]:
    return self.events


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
  assert 'does not contain "take off"' in exc_info.value.detail
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
  assert 'does not contain "take off"' in exc_info.value.detail


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

  async def fake_resolve_requested_subagent_backend_model(
      session_id: str,
      cfg: Any,
      mgr: Any,
      requested_backend: Optional[str] = None,
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
      request: Optional[SpawnRequest] = None,
  ) -> None:
    assert mgr is session_mgr
    assert t_mgr is thread_mgr

  def fake_create_logged_task(coro: Any, *, name: Optional[str] = None) -> Any:
    del name
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()
    return object()

  session_mgr.load_chat_events_sync = fail_if_gate_runs  # type: ignore[method-assign]
  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(internal, "create_logged_task", fake_create_logged_task)
  monkeypatch.setattr(internal, "get_config", lambda: object())

  result = await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result == {"thread_id": "thread-id", "description": req.description}
  assert thread_mgr.create_thread.call_args.kwargs["require_review"] is False
  assert captured["request"] == SpawnRequest(
      repo_path=None,
      base_branch=None,
      context=req.context,
      resolved_backend="codex-o3",
      resolved_model="o3",
      require_takeoff=False,
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
async def test_delegate_task_passes_require_takeoff_to_spawn_worker(monkeypatch: pytest.MonkeyPatch) -> None:
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

  async def fake_resolve_requested_subagent_backend_model(
      session_id: str,
      cfg: Any,
      mgr: Any,
      requested_backend: Optional[str] = None,
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
      request: Optional[SpawnRequest] = None,
  ) -> None:
    assert mgr is session_mgr
    assert t_mgr is thread_mgr

  def fake_create_logged_task(coro: Any, *, name: Optional[str] = None) -> Any:
    del name
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()
    return object()

  monkeypatch.setattr(internal, "check_takeoff_gate", fake_takeoff_gate)
  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(internal, "create_logged_task", fake_create_logged_task)
  monkeypatch.setattr(internal, "get_config", lambda: object())

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
      require_takeoff=True,
      task_type=TaskType.IMPLEMENT,
  )
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
