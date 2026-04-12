"""Regression tests for /api/internal/delegate takeoff gate behavior."""

from typing import Any, Optional
from unittest.mock import AsyncMock

import pytest
from fastapi import HTTPException

from src.api import internal
from src.core.models import DelegateRequest, SessionMetadata, SpawnRequest, ThreadMetadata
from src.core.spawner import DelegationBlockedError


def _build_request() -> DelegateRequest:
  return DelegateRequest(
      session_id="session-id",
      description="Do work",
      base_branch="main",
      backend="codex-o3",
      repo_path="/tmp/repo",
  )


@pytest.mark.asyncio
async def test_delegate_task_returns_403_when_takeoff_gate_blocks(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()

  def fake_check_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr
    raise DelegationBlockedError("blocked")

  monkeypatch.setattr(internal, "_check_takeoff_gate", fake_check_takeoff_gate)

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 403
  assert exc_info.value.detail == "blocked"
  thread_mgr.create_thread.assert_not_awaited()
  session_mgr.persist_and_broadcast.assert_not_awaited()


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

  def fake_check_takeoff_gate(session_id: str, mgr: Any) -> None:
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

  monkeypatch.setattr(internal, "_check_takeoff_gate", fake_check_takeoff_gate)
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
  )
  session_mgr.persist_and_broadcast.assert_awaited_once()


@pytest.mark.asyncio
async def test_delegate_task_returns_400_for_invalid_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()

  def fake_check_takeoff_gate(session_id: str, mgr: Any) -> None:
    assert session_id == req.session_id
    assert mgr is session_mgr

  async def fake_resolve_requested_subagent_backend_model(*args: Any, **kwargs: Any) -> tuple[str, str]:
    raise ValueError("requested backend 'codex-o3' is not in backend_options")

  monkeypatch.setattr(internal, "_check_takeoff_gate", fake_check_takeoff_gate)
  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "get_config", lambda: object())

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'codex-o3' is not in backend_options"
  thread_mgr.create_thread.assert_not_awaited()
