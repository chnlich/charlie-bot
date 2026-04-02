"""Tests for src/cli/improve.py and the /api/internal/improve endpoint."""

import json
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from fastapi import HTTPException

from src.cli.improve import main
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption


def _mock_config(tmp_path: Path):
  """Create a mock config with sessions_dir."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.server_port = 9443
  cfg.server_base_url = "http://localhost:9443"
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
          "--iterations",
          "2",
          "--goal",
          "optimize",
          "--base-branch",
          "main",
          "--backend",
          "codex-o3",
      ],
  ), \
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
  assert payload["iterations"] == 2
  assert payload["goal"] == "optimize"
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"


def test_main_exits_on_request_error(tmp_path: Path):
  """main() exits with code 1 on request failure."""
  cfg = _mock_config(tmp_path)

  import requests as req_lib
  with patch("sys.argv", ["improve", "--session", "s1", "--repo", "/tmp/repo", "--goal", "fix", "--base-branch", "main"]), \
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
      iterations=3,
      goal="optimize",
      backend="codex-o3",
  )

  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = MagicMock(backend="claude-opus-4.6")  # session exists

  thread_mgr = AsyncMock()
  captured: dict[str, object] = {}

  cfg = CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-improve-endpoint"),
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
          BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
      ],
  )

  def fake_create_logged_task(coro, *, name=None):
    del name
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()
    return object()

  with patch("src.api.internal.get_config", return_value=cfg), \
       patch("src.api.internal._check_takeoff_gate", return_value=None), \
       patch("src.api.internal.create_logged_task", side_effect=fake_create_logged_task) as mock_create_task:
    result = await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result["status"] == "started"
  assert result["session_id"] == "s1"
  assert result["iterations"] == 3
  mock_create_task.assert_called_once()
  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"


@pytest.mark.asyncio
async def test_improve_endpoint_returns_404_for_missing_session():
  """POST /api/internal/improve returns 404 when session doesn't exist."""
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
async def test_improve_endpoint_returns_400_for_invalid_requested_backend(tmp_path: Path):
  """POST /api/internal/improve rejects unknown backend ids with a clear error."""
  from src.api.internal import start_improve_loop
  from src.core.models import ImproveRequest

  req = ImproveRequest(
      session_id="s1",
      repo_path="/tmp/repo",
      base_branch="main",
      iterations=1,
      goal="fix",
      backend="missing-backend",
  )

  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = MagicMock(backend="claude-opus-4.6")
  thread_mgr = AsyncMock()
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
      ],
  )

  with patch("src.api.internal.get_config", return_value=cfg), \
       patch("src.api.internal._check_takeoff_gate", return_value=None):
    with pytest.raises(HTTPException) as exc_info:
      await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'missing-backend' is not in backend_options"
