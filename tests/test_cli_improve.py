"""Tests for src/features/improve/cli.py, src/features/improve/stop_cli.py, and the /api/internal/improve endpoints."""
import pathlib
from unittest import mock

import conftest
import pytest

from src.features.improve import api as improve_api
from src.features.improve import cli as improve
from src.features.improve import improve_command
from src.features.improve import stop_cli as improve_stop
from src.runtime.cli import common

_INTERNAL_GET_CONFIG_PATCH_TARGET = "src.runtime.api.internal.get_config"
_INTERNAL_CHECK_TAKEOFF_GATE_PATCH_TARGET = "src.runtime.api.internal.check_takeoff_gate"
_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET = "src.runtime.spawner_backends.resolve_requested_subagent_backend_model"
_IMPROVE_API_RESERVE_LOOP_STATE_PATCH_TARGET = "src.features.improve.api.reserve_loop_state"


def _improve_argv(session_id: str | None, repo: str, goal_file: pathlib.Path, *extra: str) -> list[str]:
  """sys.argv stand-in for improve main(): the session/repo/goal-file wiring every test shares."""
  argv = ["improve"]
  if session_id is not None:
    argv += ["--session", session_id]
  argv += ["--repo", repo, "--base-branch", "main", "--goal-file", str(goal_file)]
  return argv + list(extra)


def test_main_posts_to_improve_endpoint(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """main() reads --goal-file and posts its content to /api/internal/improve."""
  cfg = conftest.make_sessions_dir_config(tmp_path)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("optimize")

  resp_mock = conftest.make_json_response({"status": "started", "session_id": "s1", "iterations": 2})

  with conftest.patched_cli_post(cfg, _improve_argv("s1", str(tmp_path), goal_file, "--backend", "codex-o3",
                                                    "--iterations", "2"), return_value=resp_mock) as post_mock:
    improve.main()

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


def test_main_posts_plan_file_when_provided(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """main() reads optional --plan-file and includes it in the improve payload."""
  cfg = conftest.make_sessions_dir_config(tmp_path)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("optimize")
  plan_file = tmp_path / "plan.md"
  plan_file.write_text("1. largest lever")

  resp_mock = conftest.make_json_response({"status": "started", "session_id": "s1", "iterations": 2})

  with conftest.patched_cli_post(cfg, _improve_argv("s1", str(tmp_path), goal_file, "--iterations", "2", "--plan-file",
                                                    str(plan_file)), return_value=resp_mock) as post_mock:
    improve.main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["goal"] == "optimize"
  assert payload["plan"] == "1. largest lever"


def test_main_exits_on_request_error(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """main() exits with code 1 on request failure."""
  cfg = conftest.make_sessions_dir_config(tmp_path)
  monkeypatch.chdir(tmp_path)

  goal_file = tmp_path / "goal.md"
  goal_file.write_text("fix")

  with conftest.patched_cli_post(cfg, _improve_argv("s1", str(tmp_path), goal_file),
                        side_effect=common._SentButLostError("conn error")), \
       mock.patch(conftest.CONFIG_GET_CONFIG_PATCH_TARGET, return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      improve.main()
    assert exc_info.value.code == 1


# ---------------------------------------------------------------------------
# Tests for the /api/internal/improve endpoint
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_improve_endpoint_returns_404_for_missing_session() -> None:
  """POST /api/internal/improve returns 404 when session doesn't exist."""
  import fastapi

  req = improve_api.ImproveRequest(
      session_id="missing", repo_path="/tmp/repo", base_branch="main", iterations=1, goal="fix")

  session_mgr = mock.AsyncMock()
  session_mgr.get_session.return_value = None

  with pytest.raises(fastapi.HTTPException) as exc_info:
    await improve_api.start_improve_loop(req, session_mgr=session_mgr)
  assert exc_info.value.status_code == 404


@pytest.mark.asyncio
async def test_improve_endpoint_returns_400_for_invalid_backend() -> None:
  """POST /api/internal/improve returns 400 when backend resolution fails."""
  import fastapi

  req = improve_api.ImproveRequest(
      session_id="s1", repo_path="/tmp/repo", base_branch="main", backend="missing", goal="fix")

  session_mgr = mock.AsyncMock()
  session_mgr.get_session.return_value = mock.MagicMock(profile=None)

  async def fake_resolve_requested_subagent_backend_model(*args: object, **kwargs: object) -> tuple[str, str]:
    raise ValueError("requested backend 'missing' is not in backends.options")

  with mock.patch(_INTERNAL_GET_CONFIG_PATCH_TARGET, return_value=mock.MagicMock()), \
       mock.patch(_INTERNAL_CHECK_TAKEOFF_GATE_PATCH_TARGET, return_value=None), \
       mock.patch(
           _RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET,
           side_effect=fake_resolve_requested_subagent_backend_model), \
       pytest.raises(fastapi.HTTPException) as exc_info:
    await improve_api.start_improve_loop(req, session_mgr=session_mgr, task_mgr=mock.AsyncMock())

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'missing' is not in backends.options"


@pytest.mark.asyncio
async def test_improve_endpoint_returns_409_for_running_loop() -> None:
  """POST /api/internal/improve returns 409 when another loop is already running."""
  import fastapi

  from src.features.improve import improve_command

  req = improve_api.ImproveRequest(
      session_id="s1",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      iterations=3,
      goal="optimize",
  )

  session_mgr = mock.AsyncMock()
  session_mgr.get_session.return_value = mock.MagicMock(profile=None)

  with mock.patch(_INTERNAL_GET_CONFIG_PATCH_TARGET, return_value=mock.MagicMock()), \
       mock.patch(_INTERNAL_CHECK_TAKEOFF_GATE_PATCH_TARGET, return_value=None), \
       mock.patch(_RESOLVE_SUBAGENT_BACKEND_MODEL_PATCH_TARGET, return_value=("codex-o3", "o3")), \
       mock.patch(
           _IMPROVE_API_RESERVE_LOOP_STATE_PATCH_TARGET,
           side_effect=improve_command.ImproveLoopAlreadyRunningError(7)), \
       pytest.raises(fastapi.HTTPException) as exc_info:
    await improve_api.start_improve_loop(req, session_mgr=session_mgr, task_mgr=mock.AsyncMock())

  assert exc_info.value.status_code == 409
  assert exc_info.value.detail == "Loop 7 is already running for this session. Use charliebot improve-stop first."


# ---------------------------------------------------------------------------
# charliebot improve-stop
# ---------------------------------------------------------------------------


def test_improve_stop_cli_posts_to_stop_endpoint_and_prints_stopped(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """improve-stop resolves the session and prints `stopped` on the server's ack."""
  cfg = conftest.make_sessions_dir_config(tmp_path)
  monkeypatch.chdir(tmp_path)

  resp_mock = conftest.make_json_response({"status": "stopped", "session_id": "s1"})

  with conftest.patched_cli_post(cfg, ["improve-stop", "--session", "s1"], return_value=resp_mock) as post_mock:
    improve_stop.main()

  post_mock.assert_called_once()
  assert post_mock.call_args.args[0].endswith("/api/internal/improve/stop")
  assert post_mock.call_args.kwargs["json"] == {"session_id": "s1"}
  assert capsys.readouterr().out == "stopped\n"


def test_improve_stop_cli_exits_1_without_a_running_loop(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """The server's no-running-loop rejection reaches the operator as exit 1."""
  cfg = conftest.make_sessions_dir_config(tmp_path)
  monkeypatch.chdir(tmp_path)

  resp_mock = conftest.make_json_response({"detail": "No active improve loop in this session"}, status_code=409)

  with conftest.patched_cli_post(cfg, ["improve-stop", "--session", "s1"], return_value=resp_mock), \
      pytest.raises(SystemExit) as exc_info:
    improve_stop.main()
  assert exc_info.value.code == 1
  assert "No active improve loop in this session" in capsys.readouterr().err


@pytest.mark.asyncio
async def test_improve_stop_endpoint_marks_loop_stopped(tmp_path: pathlib.Path) -> None:
  """POST /api/internal/improve/stop stops the running loop; the session takes a new loop."""
  cfg = mock.MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)

  state = await improve_command.reserve_loop_state("s1", "optimize", "improve/test", str(tmp_path), cfg)
  session_mgr = mock.AsyncMock()
  session_mgr.get_session.return_value = mock.MagicMock()

  req = improve_api.ImproveStopRequest(session_id="s1")
  resp = await improve_api.stop_improve(req, cfg=cfg, session_mgr=session_mgr)
  assert resp == {"status": "stopped", "session_id": "s1"}

  saved = await improve_command.load_loop_state("s1", state.loop_id, cfg)
  assert saved is not None and saved.status == "stopped"


@pytest.mark.asyncio
async def test_improve_stop_endpoint_409_without_running_loop(tmp_path: pathlib.Path) -> None:
  """A session with no running loop answers 409 instead of pretending to stop."""
  import fastapi

  cfg = mock.MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  session_mgr = mock.AsyncMock()
  session_mgr.get_session.return_value = mock.MagicMock()

  req = improve_api.ImproveStopRequest(session_id="s1")
  with pytest.raises(fastapi.HTTPException) as exc_info:
    await improve_api.stop_improve(req, cfg=cfg, session_mgr=session_mgr)
  assert exc_info.value.status_code == 409
  assert exc_info.value.detail == "No active improve loop in this session"


@pytest.mark.asyncio
async def test_improve_stop_endpoint_404_for_missing_session(tmp_path: pathlib.Path) -> None:
  """A missing session answers 404 like the other session-scoped internal routes."""
  import fastapi

  cfg = mock.MagicMock()
  session_mgr = mock.AsyncMock()
  session_mgr.get_session.return_value = None

  req = improve_api.ImproveStopRequest(session_id="missing")
  with pytest.raises(fastapi.HTTPException) as exc_info:
    await improve_api.stop_improve(req, cfg=cfg, session_mgr=session_mgr)
  assert exc_info.value.status_code == 404


@pytest.mark.asyncio
async def test_session_takes_a_new_loop_after_stop(tmp_path: pathlib.Path) -> None:
  """After a stop, the session's active lock releases (the sequence's own exit
  step, src/features/improve/improve_sequence.py) and a new loop reserves cleanly."""
  from src.features.improve import improve_command

  cfg = mock.MagicMock()
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)

  state = await improve_command.reserve_loop_state("s1", "optimize", "improve/test", str(tmp_path), cfg)
  await improve_command.stop_improve_loop("s1", cfg)
  # The running controller's exit path releases the lock when the sequence ends.
  await improve_command.clear_active_loop_lock("s1", cfg)

  second = await improve_command.reserve_loop_state("s1", "optimize again", "improve/test-2", str(tmp_path), cfg)
  assert second.loop_id != state.loop_id
