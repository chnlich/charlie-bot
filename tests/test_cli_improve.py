"""Tests for src/cli/improve.py and the /api/internal/improve endpoint."""

import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from src.cli.improve import main


def _mock_config(tmp_path: Path):
  """Create a mock config with sessions_dir."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.server_port = 9443
  return cfg


def test_main_posts_to_improve_endpoint(tmp_path: Path):
  """main() posts to /api/internal/improve and returns immediately."""
  cfg = _mock_config(tmp_path)

  resp_mock = MagicMock()
  resp_mock.json.return_value = {"status": "started", "session_id": "s1", "iterations": 2}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "improve",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--backend",
          "codex-o3",
          "--iterations",
          "2",
          "--goal",
          "optimize",
      ]), \
       patch("src.cli.improve.get_config", return_value=cfg), \
       patch("src.cli.improve.requests.post", return_value=resp_mock) as post_mock:
    main()

  # Should have posted exactly once to the improve endpoint
  post_mock.assert_called_once()
  call_args = post_mock.call_args
  assert "/api/internal/improve" in call_args[0][0]
  payload = call_args[1]["json"]
  assert payload["session_id"] == "s1"
  assert payload["repo_path"] == "/tmp/repo"
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"
  assert payload["iterations"] == 2
  assert payload["goal"] == "optimize"


def test_main_exits_on_request_error(tmp_path: Path):
  """main() exits with code 1 on request failure."""
  cfg = _mock_config(tmp_path)

  import requests as req_lib
  with patch("sys.argv", ["improve", "--session", "s1", "--repo", "/tmp/repo", "--base-branch", "main", "--goal", "fix"]), \
       patch("src.cli.improve.get_config", return_value=cfg), \
       patch("src.cli.improve.requests.post", side_effect=req_lib.RequestException("conn error")):
    with pytest.raises(SystemExit) as exc_info:
      main()
    assert exc_info.value.code == 1


# ---------------------------------------------------------------------------
# Tests for the /api/internal/improve endpoint
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_improve_endpoint_creates_background_task():
  """POST /api/internal/improve returns immediately and creates a background task."""
  from src.api.internal import start_improve_loop
  from src.core.models import ImproveRequest

  req = ImproveRequest(
      session_id="s1",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      iterations=3,
      goal="optimize",
  )

  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = MagicMock()  # session exists

  thread_mgr = AsyncMock()

  captured: dict[str, object] = {}

  async def fake_resolve_requested_subagent_backend_model(
      session_id: str,
      cfg: object,
      mgr: object,
      requested_backend: str | None = None,
  ) -> tuple[str, str]:
    assert session_id == "s1"
    assert mgr is session_mgr
    assert requested_backend == "codex-o3"
    return "codex-o3", "o3"

  def fake_create_logged_task(coro: object, *, name: str | None = None) -> object:
    del name
    if getattr(coro, "cr_frame", None) is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()
    return object()

  with patch("src.api.internal.get_config") as mock_cfg, \
       patch("src.api.internal._check_takeoff_gate", return_value=None), \
       patch(
           "src.api.internal.resolve_requested_subagent_backend_model",
           side_effect=fake_resolve_requested_subagent_backend_model), \
       patch("src.api.internal.find_running_loop", return_value=None), \
       patch("src.api.internal.create_logged_task", side_effect=fake_create_logged_task) as mock_create_task:
    mock_cfg.return_value = MagicMock()
    result = await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result["status"] == "started"
  assert result["session_id"] == "s1"
  assert result["iterations"] == 3
  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"
  mock_create_task.assert_called_once()


@pytest.mark.asyncio
async def test_improve_endpoint_returns_404_for_missing_session():
  """POST /api/internal/improve returns 404 when session doesn't exist."""
  from fastapi import HTTPException

  from src.api.internal import start_improve_loop
  from src.core.models import ImproveRequest

  req = ImproveRequest(session_id="missing", repo_path="/tmp/repo", base_branch="main", iterations=1, goal="fix")

  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = None

  thread_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)
  assert exc_info.value.status_code == 404


@pytest.mark.asyncio
async def test_improve_endpoint_returns_400_for_invalid_backend():
  """POST /api/internal/improve returns 400 when backend resolution fails."""
  from fastapi import HTTPException

  from src.api.internal import start_improve_loop
  from src.core.models import ImproveRequest

  req = ImproveRequest(session_id="s1", repo_path="/tmp/repo", base_branch="main", backend="missing", goal="fix")

  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = MagicMock()
  thread_mgr = AsyncMock()

  async def fake_resolve_requested_subagent_backend_model(*args: object, **kwargs: object) -> tuple[str, str]:
    raise ValueError("requested backend 'missing' is not in backend_options")

  with patch("src.api.internal._check_takeoff_gate", return_value=None), \
       patch(
           "src.api.internal.resolve_requested_subagent_backend_model",
           side_effect=fake_resolve_requested_subagent_backend_model):
    with pytest.raises(HTTPException) as exc_info:
      await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'missing' is not in backend_options"


@pytest.mark.asyncio
async def test_improve_endpoint_returns_409_for_running_loop():
  """POST /api/internal/improve returns 409 when another loop is already running."""
  from fastapi import HTTPException

  from src.api.internal import start_improve_loop
  from src.core.models import ImproveRequest

  req = ImproveRequest(
      session_id="s1",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      iterations=3,
      goal="optimize",
  )

  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = MagicMock()
  thread_mgr = AsyncMock()

  with patch("src.api.internal.get_config", return_value=MagicMock()), \
       patch("src.api.internal._check_takeoff_gate", return_value=None), \
       patch("src.api.internal.resolve_requested_subagent_backend_model", return_value=("codex-o3", "o3")), \
       patch("src.api.internal.find_running_loop", return_value=MagicMock(loop_id=7)):
    with pytest.raises(HTTPException) as exc_info:
      await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 409
  assert exc_info.value.detail == "Loop 7 is already running for this session. Use /stop-improve first."
