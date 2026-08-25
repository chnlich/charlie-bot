"""Tests for src/cli/improve.py and the /api/internal/improve endpoint."""
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import setup_session_cwd as _setup_session_cwd
from pydantic import ValidationError

from src.cli.improve import main
from src.core.models import ImproveRequest


def _mock_config(tmp_path: Path):
  """Create a mock config with sessions_dir."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  cfg.server_port = 9443
  return cfg


def test_main_posts_to_improve_endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """main() reads --goal-file and posts its content to /api/internal/improve."""
  cfg = _mock_config(tmp_path)
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("optimize")

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
          str(tmp_path),
          "--base-branch",
          "main",
          "--backend",
          "codex-o3",
          "--iterations",
          "2",
          "--goal-file",
          str(goal_file),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  # Should have posted exactly once to the improve endpoint
  post_mock.assert_called_once()
  call_args = post_mock.call_args
  assert "/api/internal/improve" in call_args[0][0]
  payload = call_args[1]["json"]
  assert payload["session_id"] == "s1"
  assert payload["repo_path"] == str(tmp_path)
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"
  assert payload["iterations"] == 2
  assert payload["goal"] == "optimize"
  assert "plan" not in payload


def test_main_posts_plan_file_when_provided(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """main() reads optional --plan-file and includes it in the improve payload."""
  cfg = _mock_config(tmp_path)
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("optimize")
  plan_file = tmp_path / "plan.md"
  plan_file.write_text("1. largest lever")

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
          str(tmp_path),
          "--base-branch",
          "main",
          "--iterations",
          "2",
          "--goal-file",
          str(goal_file),
          "--plan-file",
          str(plan_file),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["goal"] == "optimize"
  assert payload["plan"] == "1. largest lever"


def test_main_exits_on_request_error(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
  """main() exits with code 1 on request failure."""
  cfg = _mock_config(tmp_path)
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")

  import requests as req_lib
  with patch(
      "sys.argv",
      ["improve", "--session", "s1", "--repo", str(tmp_path), "--base-branch", "main", "--goal-file", str(goal_file)]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", side_effect=req_lib.RequestException("conn error")):
    with pytest.raises(SystemExit) as exc_info:
      main()
    assert exc_info.value.code == 1


def test_session_auto_derived_from_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"status": "started"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "improve",
          "--repo",
          str(tmp_path),
          "--base-branch",
          "main",
          "--goal-file",
          str(goal_file),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"


def test_session_matches_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"status": "started"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "improve",
          "--session",
          "abc",
          "--repo",
          str(tmp_path),
          "--base-branch",
          "main",
          "--goal-file",
          str(goal_file),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"


def test_session_mismatch_with_cwd_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")

  with patch(
      "sys.argv",
      [
          "improve",
          "--session",
          "xyz",
          "--repo",
          str(tmp_path),
          "--base-branch",
          "main",
          "--goal-file",
          str(goal_file),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "mismatch" in err
  assert "abc" in err
  assert "xyz" in err


def test_no_session_outside_session_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")

  with patch(
      "sys.argv",
      [
          "improve",
          "--repo",
          str(tmp_path),
          "--base-branch",
          "main",
          "--goal-file",
          str(goal_file),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "session dir" in err or "--session required" in err


def test_main_rejects_missing_goal_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A nonexistent --goal-file exits non-zero before any request is made."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  missing = tmp_path / "nope.md"

  with patch(
      "sys.argv",
      ["improve", "--repo", str(tmp_path), "--base-branch", "main", "--goal-file", str(missing)]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "goal-file" in err and "not found" in err


def test_main_rejects_empty_goal_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A whitespace-only --goal-file exits non-zero before any request is made."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  empty = tmp_path / "empty.md"
  empty.write_text("   \n")

  with patch(
      "sys.argv",
      ["improve", "--repo", str(tmp_path), "--base-branch", "main", "--goal-file", str(empty)]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "empty" in err


def test_main_rejects_missing_plan_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A nonexistent --plan-file exits non-zero before any request is made."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")
  missing = tmp_path / "nope-plan.md"

  with patch(
      "sys.argv",
      [
          "improve",
          "--repo",
          str(tmp_path),
          "--base-branch",
          "main",
          "--goal-file",
          str(goal_file),
          "--plan-file",
          str(missing),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "plan-file" in err and "not found" in err


def test_main_rejects_empty_plan_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A whitespace-only --plan-file exits non-zero before any request is made."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")
  empty = tmp_path / "empty-plan.md"
  empty.write_text("   \n")

  with patch(
      "sys.argv",
      [
          "improve",
          "--repo",
          str(tmp_path),
          "--base-branch",
          "main",
          "--goal-file",
          str(goal_file),
          "--plan-file",
          str(empty),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "plan-file" in err and "empty" in err


def test_main_rejects_relative_repo_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """A relative --repo is rejected before any network call to the internal API."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")

  with patch(
      "sys.argv",
      [
          "improve",
          "--session",
          "s1",
          "--repo",
          "meshy-research",
          "--base-branch",
          "main",
          "--goal-file",
          str(goal_file),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "must be an absolute path" in err
  assert "meshy-research" in err


def test_main_rejects_nonexistent_repo_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """An absolute but non-existent --repo is rejected before any network call to the internal API."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")
  nonexistent = str(tmp_path / "nonexistent")

  with patch(
      "sys.argv",
      [
          "improve",
          "--session",
          "s1",
          "--repo",
          nonexistent,
          "--base-branch",
          "main",
          "--goal-file",
          str(goal_file),
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "does not exist" in err
  assert nonexistent in err


# ---------------------------------------------------------------------------
# Tests for the /api/internal/improve endpoint
# ---------------------------------------------------------------------------


def test_improve_request_rejects_branch_prefix():
  """ImproveRequest fails fast on the removed branch_prefix field."""
  with pytest.raises(ValidationError):
    ImproveRequest(
        session_id="s1",
        repo_path="/tmp/repo",
        base_branch="main",
        iterations=1,
        goal="fix",
        branch_prefix="improve/old",
    )


@pytest.mark.asyncio
async def test_improve_endpoint_creates_background_task(tmp_path: Path):
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
      plan="1. largest lever",
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

  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"

  with patch("src.api.internal.get_config") as mock_cfg, \
       patch("src.api.internal.check_takeoff_gate", return_value=None), \
       patch(
           "src.api.internal.resolve_requested_subagent_backend_model",
           side_effect=fake_resolve_requested_subagent_backend_model), \
       patch(
           "src.api.internal.reserve_loop_state",
           return_value=MagicMock(loop_id=11)) as mock_reserve, \
       patch("src.api.internal.create_logged_task", side_effect=fake_create_logged_task) as mock_create_task:
    mock_cfg.return_value = cfg
    result = await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert result["status"] == "started"
  assert result["session_id"] == "s1"
  assert result["iterations"] == 3
  assert result["plan_path"] == str(tmp_path / "sessions" / "s1" / "loops" / "11" / "plan.md")
  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"
  assert captured["loop_id"] == 11
  assert captured["plan"] == "1. largest lever"
  assert mock_reserve.call_args.kwargs["plan"] == "1. largest lever"
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

  with patch("src.api.internal.check_takeoff_gate", return_value=None), \
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
  from src.core.improve_command import ImproveLoopAlreadyRunningError
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
       patch("src.api.internal.check_takeoff_gate", return_value=None), \
       patch("src.api.internal.resolve_requested_subagent_backend_model", return_value=("codex-o3", "o3")), \
       patch(
           "src.api.internal.reserve_loop_state",
           side_effect=ImproveLoopAlreadyRunningError(7)):
    with pytest.raises(HTTPException) as exc_info:
      await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 409
  assert exc_info.value.detail == "Loop 7 is already running for this session. Use /stop-improve first."
