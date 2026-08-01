"""Tests for src/cli/plan.py — argument validation, session resolution, stdout/stderr shape."""

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests

from src.cli.plan import _PLAN_REMINDER, main


def _setup_session_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, sid: str) -> MagicMock:
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  session_dir = cfg.sessions_dir / sid
  session_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(session_dir)
  return cfg


def _mock_response(payload: dict) -> MagicMock:
  resp = MagicMock()
  resp.json.return_value = payload
  resp.raise_for_status = MagicMock()
  return resp


def test_plan_present_posts_to_present_endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plan": 1, "v": 1, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "present",
      "--file", "artifacts/plan_01.html",
      "--title", "P1",
  ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"
  assert payload["file"] == "artifacts/plan_01.html"
  assert "verify_thread" not in payload
  assert payload["title"] == "P1"
  assert payload["base_repo"] is None
  assert payload["base_branch"] is None
  assert payload["base_sha"] is None


def test_plan_present_passes_base(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plan": 1, "v": 1, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "present",
      "--file", "artifacts/plan_01.html",
      "--title", "P1",
      "--base-repo", "r",
      "--base-branch", "b",
      "--base-sha", "s",
  ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["base_repo"] == "r"
  assert payload["base_branch"] == "b"
  assert payload["base_sha"] == "s"


def test_plan_amend_posts_with_default_trigger(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plan": 1, "v": 2, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "amend",
      "--file", "artifacts/plan_02.html",
  ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"
  assert payload["file"] == "artifacts/plan_02.html"
  assert "verify_thread" not in payload
  assert payload["plan_id"] is None
  assert payload["trigger"] == "feedback"

  out = capsys.readouterr().out
  assert json.loads(out)["reminder"] == _PLAN_REMINDER


def test_plan_amend_passes_plan_and_trigger(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plan": 2, "v": 3, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "amend",
      "--file", "artifacts/plan_03.html",
      "--plan", "2",
      "--trigger", "auto_amend",
  ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["plan_id"] == 2
  assert payload["trigger"] == "auto_amend"


def test_plan_approve_posts_plan_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plan": 1, "v": 1, "state": "approved"})
  with patch("sys.argv", ["plan", "approve", "--plan", "1"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload == {"session_id": "abc", "plan_id": 1}

  out = capsys.readouterr().out
  assert "reminder" not in json.loads(out)


def test_plan_close_posts(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plan": 1, "state": "superseded"})
  with patch("sys.argv", [
      "plan", "close",
      "--plan", "1",
      "--as", "superseded",
  ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload == {"session_id": "abc", "plan_id": 1, "close_as": "superseded"}


def test_plan_list_uses_get_endpoint(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plans": []})
  with patch("sys.argv", ["plan", "list"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.get", return_value=resp) as get_mock:
    main()

  assert get_mock.call_args.args[0].endswith("/api/sessions/abc/plans")

  out = capsys.readouterr().out
  assert "reminder" not in json.loads(out)


def test_plan_list_corrupt_registry_prints_errors_and_exits_0(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """Acceptance #4: CLI plan list on a corrupt registry prints the errors entries and exits 0."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  payload = {
      "plans": [],
      "errors": [{
          "session_id": "abc",
          "plan_id": None,
          "error": "Expecting value: line 1 column 1 (char 0)"
      }],
  }
  resp = _mock_response(payload)
  with patch("sys.argv", ["plan", "list"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.get", return_value=resp):
    main()  # no SystemExit — exits 0

  out = capsys.readouterr().out
  parsed = json.loads(out)
  assert parsed["plans"] == []
  assert len(parsed["errors"]) == 1
  assert parsed["errors"][0]["session_id"] == "abc"
  assert parsed["errors"][0]["plan_id"] is None
  assert "Expecting value" in parsed["errors"][0]["error"]


def test_plan_present_stdout_json(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plan": 1, "v": 1, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "present",
      "--file", "artifacts/plan_01.html",
      "--title", "P1",
  ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp):
    main()

  out = capsys.readouterr().out
  parsed = json.loads(out)
  assert parsed == {"plan": 1, "v": 1, "state": "awaiting approval", "reminder": _PLAN_REMINDER}


def test_plan_server_rejection_exits_nonzero_with_detail_on_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")

  class FakeRequestException(requests.RequestException):

    def __init__(self) -> None:
      super().__init__("bad")
      self.response = MagicMock()
      self.response.json.return_value = {
          "detail": "file 'artifacts/missing.html' not found inside the session directory"
      }

  with patch("sys.argv", [
      "plan", "present",
      "--file", "artifacts/missing.html",
      "--title", "P1",
  ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", side_effect=FakeRequestException()):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 1
  err = capsys.readouterr().err
  parsed = json.loads(err)
  assert parsed["error"] == "file 'artifacts/missing.html' not found inside the session directory"


def test_plan_session_auto_derived_from_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = _mock_response({"plans": []})
  with patch("sys.argv", ["plan", "list"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.get", return_value=resp) as get_mock:
    main()

  assert "/api/sessions/abc/plans" in get_mock.call_args.args[0]


def test_plan_session_mismatch_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  with patch("sys.argv", [
      "plan", "list", "--session", "xyz",
  ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "mismatch" in err


def test_plan_no_session_outside_session_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(tmp_path)
  with patch("sys.argv", ["plan", "list"]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "--session required" in err or "session dir" in err


def test_plan_present_requires_file(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["plan", "present", "--title", "P1"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_present_requires_title(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["plan", "present", "--file", "f.html"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_close_requires_plan(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["plan", "close", "--as", "superseded"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_close_requires_as(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["plan", "close", "--plan", "1"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_close_rejects_invalid_as(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["plan", "close", "--plan", "1", "--as", "weird"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_amend_rejects_invalid_trigger(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", [
      "plan",
      "amend",
      "--file",
      "f.html",
      "--trigger",
      "initial",
  ]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_reverify_subcommand_removed(capsys: pytest.CaptureFixture[str]) -> None:
  """The reverify subcommand is gone; argparse rejects it with exit code 2."""
  with patch("sys.argv", ["plan", "reverify", "--verify-thread", "t2", "--plan", "1"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_present_rejects_verify_thread_arg(capsys: pytest.CaptureFixture[str]) -> None:
  """--verify-thread is no longer a recognized argument on present."""
  with patch("sys.argv", ["plan", "present", "--file", "f.html", "--verify-thread", "t1", "--title", "P1"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_amend_rejects_verify_thread_arg(capsys: pytest.CaptureFixture[str]) -> None:
  """--verify-thread is no longer a recognized argument on amend."""
  with patch("sys.argv", ["plan", "amend", "--file", "f.html", "--verify-thread", "t1"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0


def test_plan_requires_verb(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["plan"]):
    with pytest.raises(SystemExit) as exc_info:
      main()
  assert exc_info.value.code != 0
