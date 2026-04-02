"""Tests for src/cli/delegate.py and the /api/internal/delegate endpoint."""

from pathlib import Path
from typing import Any, Optional
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi import HTTPException

from src.api import internal
from src.cli.delegate import main
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, DelegateRequest, SessionMetadata, ThreadMetadata
from src.core.spawner import DelegationBlockedError


def _mock_config(tmp_path: Path):
  cfg = MagicMock()
  cfg.server_base_url = "http://localhost:9443"
  return cfg


def _build_request(backend: Optional[str] = None) -> DelegateRequest:
  return DelegateRequest(
      session_id="session-id",
      description="Do work",
      base_branch="main",
      backend=backend,
      repo_path="/tmp/repo",
  )


def test_delegate_cli_includes_backend_in_payload(tmp_path: Path) -> None:
  cfg = _mock_config(tmp_path)

  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "thread-id", "description": "Do work"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "session-id",
          "--repo",
          "/tmp/repo",
          "--description",
          "Do work",
          "--base-branch",
          "main",
          "--backend",
          "codex-o3",
      ],
  ), \
       patch("src.cli.delegate.get_config", return_value=cfg), \
       patch("src.cli.delegate.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "session-id"
  assert payload["repo_path"] == "/tmp/repo"
  assert payload["description"] == "Do work"
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"


@pytest.mark.asyncio
async def test_delegate_task_returns_403_when_takeoff_gate_blocks(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()

  def fake_check_takeoff_gate(session_id: str) -> None:
    assert session_id == req.session_id
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
  req = _build_request(backend="codex-o3")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test")
  thread_mgr = AsyncMock()
  thread_mgr.create_thread.return_value = ThreadMetadata(
      id="thread-id",
      session_id=req.session_id,
      description=req.description,
  )

  captured: dict[str, Any] = {}

  def fake_check_takeoff_gate(session_id: str) -> None:
    assert session_id == req.session_id

  async def fake_resolve_session_subagent_backend_model(
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
      repo_path: Optional[str] = None,
      context: Optional[str] = None,
      prompt_override: Optional[str] = None,
      resolved_backend: str = "",
      resolved_model: str = "",
      base_branch: Optional[str] = None,
      branch_name_override: Optional[str] = None,
      improve_dir: Optional[str] = None,
      iteration_number: Optional[int] = None,
      require_takeoff: bool = False,
  ) -> None:
    captured["session_id"] = session_id
    captured["description"] = description
    captured["thread_id"] = thread_id
    captured["repo_path"] = repo_path
    captured["base_branch"] = base_branch
    captured["resolved_backend"] = resolved_backend
    captured["resolved_model"] = resolved_model
    captured["require_takeoff"] = require_takeoff
    assert mgr is session_mgr
    assert t_mgr is thread_mgr

  def fake_create_logged_task(coro: Any, *, name: Optional[str] = None) -> Any:
    del name
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()
    return object()

  monkeypatch.setattr(internal, "_check_takeoff_gate", fake_check_takeoff_gate)
  monkeypatch.setattr(internal, "resolve_session_subagent_backend_model", fake_resolve_session_subagent_backend_model)
  monkeypatch.setattr(internal, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(internal, "create_logged_task", fake_create_logged_task)
  monkeypatch.setattr(internal, "get_config", lambda: object())

  result = await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result == {"thread_id": "thread-id", "description": req.description}
  assert captured["session_id"] == req.session_id
  assert captured["description"] == req.description
  assert captured["thread_id"] == "thread-id"
  assert captured["repo_path"] == req.repo_path
  assert captured["base_branch"] == req.base_branch
  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"
  assert captured["require_takeoff"] is True
  session_mgr.persist_and_broadcast.assert_awaited_once()


@pytest.mark.asyncio
async def test_delegate_task_returns_400_for_invalid_requested_backend(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  req = _build_request(backend="missing-backend")
  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = SessionMetadata(id=req.session_id, name="Test", backend="claude-opus-4.6")
  thread_mgr = AsyncMock()
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
      ],
  )

  def fake_check_takeoff_gate(session_id: str) -> None:
    assert session_id == req.session_id

  monkeypatch.setattr(internal, "_check_takeoff_gate", fake_check_takeoff_gate)
  monkeypatch.setattr(internal, "get_config", lambda: cfg)

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'missing-backend' is not in backend_options"
  thread_mgr.create_thread.assert_not_awaited()
