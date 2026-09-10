"""Tests for src/cli/improve.py and the /api/internal/improve endpoint."""
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from conftest import assert_cli_reject, capture_create_logged_task, make_json_response, patched_cli_post
from conftest import setup_session_cwd as _setup_session_cwd
from pydantic import ValidationError

from src.cli.improve import main
from src.core.models import ImproveRequest

_INTERNAL_GET_CONFIG_PATCH_TARGET = "src.api.internal.get_config"
_IMPROVE_GET_CONFIG_PATCH_TARGET = "src.cli.improve.get_config"
_INTERNAL_CHECK_TAKEOFF_GATE_PATCH_TARGET = "src.api.internal.check_takeoff_gate"
_INTERNAL_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET = ("src.api.internal.resolve_requested_subagent_backend_model")
_INTERNAL_RESERVE_LOOP_STATE_PATCH_TARGET = "src.api.internal.reserve_loop_state"


def _mock_config(tmp_path: Path):
  """Create a mock config with sessions_dir."""
  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


def _improve_argv(session_id: str | None, repo: str, goal_file: Path, *extra: str) -> list[str]:
  """sys.argv stand-in for improve main(): the session/repo/goal-file wiring every test shares."""
  argv = ["improve"]
  if session_id is not None:
    argv += ["--session", session_id]
  argv += ["--repo", repo, "--base-branch", "main", "--goal-file", str(goal_file)]
  return argv + list(extra)


def test_main_posts_to_improve_endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """main() reads --goal-file and posts its content to /api/internal/improve."""
  cfg = _mock_config(tmp_path)
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("optimize")

  resp_mock = make_json_response({"status": "started", "session_id": "s1", "iterations": 2})

  with patched_cli_post(cfg, _improve_argv("s1", str(tmp_path), goal_file, "--backend", "codex-o3", "--iterations",
                                           "2"), return_value=resp_mock) as post_mock:
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


def test_main_posts_plan_file_when_provided(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """main() reads optional --plan-file and includes it in the improve payload."""
  cfg = _mock_config(tmp_path)
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("optimize")
  plan_file = tmp_path / "plan.md"
  plan_file.write_text("1. largest lever")

  resp_mock = make_json_response({"status": "started", "session_id": "s1", "iterations": 2})

  with patched_cli_post(cfg, _improve_argv("s1", str(tmp_path), goal_file, "--iterations", "2", "--plan-file",
                                           str(plan_file)), return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["goal"] == "optimize"
  assert payload["plan"] == "1. largest lever"


def test_main_exits_on_request_error(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """main() exits with code 1 on request failure."""
  cfg = _mock_config(tmp_path)
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")

  import requests as req_lib
  with patched_cli_post(cfg, _improve_argv("s1", str(tmp_path), goal_file),
                        side_effect=req_lib.RequestException("conn error")), \
       patch(_IMPROVE_GET_CONFIG_PATCH_TARGET, return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()
    assert exc_info.value.code == 1


# One row per bad --goal-file/--plan-file shape: which files to seed (name -> content,
# absent = leave nonexistent), which file --goal-file names, which --plan-file to append
# (None = no --plan-file), and the stderr fragments main() must print.
_REJECT_BAD_FILE_ROWS = [
    pytest.param({}, "nope.md", None, ("goal-file", "not found"), id="goal-file-missing"),
    pytest.param({"empty.md": "   \n"}, "empty.md", None, ("empty",), id="goal-file-empty"),
    pytest.param({"goal.md": "fix"}, "goal.md", "nope-plan.md", ("plan-file", "not found"), id="plan-file-missing"),
    pytest.param(
        {
            "goal.md": "fix",
            "empty-plan.md": "   \n"
        },
        "goal.md",
        "empty-plan.md", ("plan-file", "empty"),
        id="plan-file-empty"),
]


@pytest.mark.parametrize(("write_files", "goal_name", "plan_name", "err_fragments"), _REJECT_BAD_FILE_ROWS)
def test_main_rejects_bad_file_before_any_request(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    write_files: dict[str, str],
    goal_name: str,
    plan_name: str | None,
    err_fragments: tuple[str, ...],
) -> None:
  """A missing or whitespace-only --goal-file/--plan-file exits non-zero before any request is made."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  for name, content in write_files.items():
    (tmp_path / name).write_text(content)
  extra = ["--plan-file", str(tmp_path / plan_name)] if plan_name is not None else []

  with patched_cli_post(cfg, _improve_argv(None, str(tmp_path), tmp_path / goal_name, *extra)) as post_mock, \
       pytest.raises(SystemExit) as exc_info:
    main()

  assert_cli_reject(exc_info, capsys, *err_fragments)
  post_mock.assert_not_called()


# ---------------------------------------------------------------------------
# Tests for the /api/internal/improve endpoint
# ---------------------------------------------------------------------------


def test_improve_request_rejects_branch_prefix() -> None:
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

  cfg = MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"

  with patch(_INTERNAL_GET_CONFIG_PATCH_TARGET) as mock_cfg, \
       patch(_INTERNAL_CHECK_TAKEOFF_GATE_PATCH_TARGET, return_value=None), \
       patch(
           _INTERNAL_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET,
           side_effect=fake_resolve_requested_subagent_backend_model), \
       patch(
           _INTERNAL_RESERVE_LOOP_STATE_PATCH_TARGET,
           return_value=MagicMock(loop_id=11)) as mock_reserve, \
       patch("src.api.internal.create_logged_task",
             side_effect=capture_create_logged_task(captured)) as mock_create_task:
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
async def test_improve_endpoint_returns_404_for_missing_session() -> None:
  """POST /api/internal/improve returns 404 when session doesn't exist."""
  from fastapi import HTTPException

  from src.api.internal import start_improve_loop

  req = ImproveRequest(session_id="missing", repo_path="/tmp/repo", base_branch="main", iterations=1, goal="fix")

  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = None

  thread_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)
  assert exc_info.value.status_code == 404


@pytest.mark.asyncio
async def test_improve_endpoint_returns_400_for_invalid_backend() -> None:
  """POST /api/internal/improve returns 400 when backend resolution fails."""
  from fastapi import HTTPException

  from src.api.internal import start_improve_loop

  req = ImproveRequest(session_id="s1", repo_path="/tmp/repo", base_branch="main", backend="missing", goal="fix")

  session_mgr = AsyncMock()
  session_mgr.get_session.return_value = MagicMock()
  thread_mgr = AsyncMock()

  async def fake_resolve_requested_subagent_backend_model(*args: object, **kwargs: object) -> tuple[str, str]:
    raise ValueError("requested backend 'missing' is not in backend_options")

  with patch(_INTERNAL_GET_CONFIG_PATCH_TARGET, return_value=MagicMock()), \
       patch(_INTERNAL_CHECK_TAKEOFF_GATE_PATCH_TARGET, return_value=None), \
       patch(
           _INTERNAL_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET,
           side_effect=fake_resolve_requested_subagent_backend_model), \
       pytest.raises(HTTPException) as exc_info:
    await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'missing' is not in backend_options"


@pytest.mark.asyncio
async def test_improve_endpoint_returns_409_for_running_loop() -> None:
  """POST /api/internal/improve returns 409 when another loop is already running."""
  from fastapi import HTTPException

  from src.api.internal import start_improve_loop
  from src.core.improve_command import ImproveLoopAlreadyRunningError

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

  with patch(_INTERNAL_GET_CONFIG_PATCH_TARGET, return_value=MagicMock()), \
       patch(_INTERNAL_CHECK_TAKEOFF_GATE_PATCH_TARGET, return_value=None), \
       patch(_INTERNAL_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET, return_value=("codex-o3", "o3")), \
       patch(
           _INTERNAL_RESERVE_LOOP_STATE_PATCH_TARGET,
           side_effect=ImproveLoopAlreadyRunningError(7)), \
       pytest.raises(HTTPException) as exc_info:
    await start_improve_loop(req, session_mgr=session_mgr, thread_mgr=thread_mgr)

  assert exc_info.value.status_code == 409
  assert exc_info.value.detail == "Loop 7 is already running for this session. Use /stop-improve first."
