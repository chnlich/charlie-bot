"""Tests for src/cli/plan.py — argument validation, session resolution, stdout/stderr shape."""

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests
from conftest import (
    CLI_COMMON_GET_CONFIG_PATCH_TARGET,
    CLI_COMMON_REQUESTS_GET_PATCH_TARGET,
    CLI_COMMON_REQUESTS_POST_PATCH_TARGET,
    make_json_response,
    plan_doc,
    plan_page_html,
    plan_version_v1,
    write_plan_artifact,
)
from conftest import setup_session_cwd as _setup_session_cwd

from src.cli.plan import _PLAN_REMINDER, main
from src.core import plan_diff


def test_plan_present_posts_to_present_endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 1, "v": 1, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "present",
      "--file", "artifacts/plan_01.html",
      "--title", "P1",
  ]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
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
  resp = make_json_response({"plan": 1, "v": 1, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "present",
      "--file", "artifacts/plan_01.html",
      "--title", "P1",
      "--base-repo", "r",
      "--base-branch", "b",
      "--base-sha", "s",
  ]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["base_repo"] == "r"
  assert payload["base_branch"] == "b"
  assert payload["base_sha"] == "s"


def test_plan_amend_posts_with_default_trigger(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 1, "v": 2, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "amend",
      "--file", "artifacts/plan_02.html",
      "--note", "folded the executor back into one",
  ]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"
  assert payload["file"] == "artifacts/plan_02.html"
  assert "verify_thread" not in payload
  assert payload["plan_id"] is None
  assert payload["trigger"] == "feedback"
  assert payload["note"] == "folded the executor back into one"

  out = capsys.readouterr().out
  assert json.loads(out)["reminder"] == _PLAN_REMINDER


def test_plan_amend_passes_plan_and_trigger(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 2, "v": 3, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "amend",
      "--file", "artifacts/plan_03.html",
      "--note", "answered verify findings",
      "--plan", "2",
      "--trigger", "auto_amend",
  ]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["plan_id"] == 2
  assert payload["trigger"] == "auto_amend"
  assert payload["note"] == "answered verify findings"


def test_plan_approve_posts_plan_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 1, "v": 1, "state": "approved"})
  with patch("sys.argv", ["plan", "approve", "--plan", "1"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload == {"session_id": "abc", "plan_id": 1}

  out = capsys.readouterr().out
  assert "reminder" not in json.loads(out)


def test_plan_close_posts(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 1, "state": "superseded"})
  with patch("sys.argv", [
      "plan", "close",
      "--plan", "1",
      "--as", "superseded",
  ]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload == {"session_id": "abc", "plan_id": 1, "close_as": "superseded"}


def test_plan_close_posts_completed(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 1, "state": "completed"})
  with patch("sys.argv", [
      "plan", "close",
      "--plan", "1",
      "--as", "completed",
  ]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload == {"session_id": "abc", "plan_id": 1, "close_as": "completed"}


def test_plan_list_uses_get_endpoint(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plans": []})
  with patch("sys.argv", ["plan", "list"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_GET_PATCH_TARGET, return_value=resp) as get_mock:
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
  resp = make_json_response(payload)
  with patch("sys.argv", ["plan", "list"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_GET_PATCH_TARGET, return_value=resp):
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
  resp = make_json_response({"plan": 1, "v": 1, "state": "awaiting approval"})
  with patch("sys.argv", [
      "plan", "present",
      "--file", "artifacts/plan_01.html",
      "--title", "P1",
  ]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, return_value=resp):
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

  with (
      patch("sys.argv", [
          "plan",
          "present",
          "--file",
          "artifacts/missing.html",
          "--title",
          "P1",
      ]),
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET, side_effect=FakeRequestException()),
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert exc_info.value.code == 1
  err = capsys.readouterr().err
  parsed = json.loads(err)
  assert parsed["error"] == "file 'artifacts/missing.html' not found inside the session directory"


def test_plan_session_auto_derived_from_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plans": []})
  with patch("sys.argv", ["plan", "list"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_GET_PATCH_TARGET, return_value=resp) as get_mock:
    main()

  assert "/api/sessions/abc/plans" in get_mock.call_args.args[0]


def test_plan_session_mismatch_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  with (
      patch("sys.argv", [
          "plan",
          "list",
          "--session",
          "xyz",
      ]),
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      pytest.raises(SystemExit) as exc_info,
  ):
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
  with (
      patch("sys.argv", ["plan", "list"]),
      patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg),
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "--session required" in err or "session dir" in err


REJECTION_CASES = [
    pytest.param(["plan", "present", "--title", "P1"], id="present-requires-file"),
    pytest.param(["plan", "present", "--file", "f.html"], id="present-requires-title"),
    pytest.param(["plan", "present", "--file", "f.html", "--title", "P1", "--note", "why"], id="present-rejects-note"),
    pytest.param(["plan", "close", "--as", "superseded"], id="close-requires-plan"),
    pytest.param(["plan", "close", "--plan", "1"], id="close-requires-as"),
    pytest.param(["plan", "close", "--plan", "1", "--as", "weird"], id="close-rejects-invalid-as"),
    pytest.param(["plan", "amend", "--file", "f.html"], id="amend-requires-note"),
    pytest.param(["plan", "amend", "--file", "f.html", "--trigger", "initial"], id="amend-rejects-invalid-trigger"),
    pytest.param(["plan", "reverify", "--verify-thread", "t2", "--plan", "1"], id="reverify-subcommand-removed"),
    pytest.param(
        ["plan", "present", "--file", "f.html", "--verify-thread", "t1", "--title", "P1"],
        id="present-rejects-verify-thread"),
    pytest.param(["plan", "amend", "--file", "f.html", "--verify-thread", "t1"], id="amend-rejects-verify-thread"),
    pytest.param(["plan"], id="requires-verb"),
]


@pytest.mark.parametrize("argv", REJECTION_CASES)
def test_plan_argparse_rejects_argv(argv: list[str]) -> None:
  """Each malformed invocation dies in argparse with a nonzero exit code."""
  with patch("sys.argv", argv), pytest.raises(SystemExit) as exc_info:
    main()
  assert exc_info.value.code != 0


# ---------------------------------------------------------------------------
# plan diff: registry through the list endpoint, diff computed locally
# ---------------------------------------------------------------------------


def _two_version_listing() -> dict:
  """A one-plan registry listing whose v2 differs from v1 (file and note), so diff_text is non-empty."""
  v2 = {**plan_version_v1("artifacts/plan_02.html"), "v": 2, "trigger": "feedback", "note": "narrowed the goal"}
  return {"plans": [plan_doc(1, [plan_version_v1("artifacts/plan_01.html"), v2])], "errors": []}


def _write_diff_pair(cfg: MagicMock) -> tuple[Path, Path]:
  """Two differing plan pages on disk under the session dir; returns their paths."""
  write_plan_artifact(cfg, "abc", "plan_01.html", plan_page_html("Ship the executor fix."))
  write_plan_artifact(cfg, "abc", "plan_02.html", plan_page_html("Ship the executor fix behind a flag."))
  return (
      cfg.sessions_dir / "abc" / "artifacts" / "plan_01.html", cfg.sessions_dir / "abc" / "artifacts" / "plan_02.html")


def test_plan_diff_prints_five_keys_computed_locally(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """--v defaults to the latest; the listing comes from the existing GET endpoint, the diff is local."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  old_path, new_path = _write_diff_pair(cfg)
  resp = make_json_response(_two_version_listing())
  with patch("sys.argv", ["plan", "diff"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_GET_PATCH_TARGET, return_value=resp) as get_mock, \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET) as post_mock:
    main()

  assert get_mock.call_args.args[0].endswith("/api/sessions/abc/plans")
  post_mock.assert_not_called()
  parsed = json.loads(capsys.readouterr().out)
  assert set(parsed.keys()) == {"plan", "from", "to", "note", "text"}
  assert parsed["plan"] == 1
  assert parsed["from"] == 1
  assert parsed["to"] == 2
  assert parsed["note"] == "narrowed the goal"
  assert parsed["text"]
  assert parsed["text"] == plan_diff.diff_text(
      old_path.read_text(encoding="utf-8"), new_path.read_text(encoding="utf-8"))


def test_plan_diff_explicit_v_names_the_pair(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  _write_diff_pair(cfg)
  resp = make_json_response(_two_version_listing())
  with patch("sys.argv", ["plan", "diff", "--v", "2"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_GET_PATCH_TARGET, return_value=resp):
    main()

  parsed = json.loads(capsys.readouterr().out)
  assert (parsed["from"], parsed["to"]) == (1, 2)


def test_plan_diff_rejects_version_1(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """Version 1 has no predecessor; the command fails with a clear message and exit code 1."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plans": [plan_doc(1, [plan_version_v1("artifacts/plan_01.html")])], "errors": []})
  with patch("sys.argv", ["plan", "diff", "--v", "1"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_GET_PATCH_TARGET, return_value=resp), \
       pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 1
  err = capsys.readouterr().err
  assert "no predecessor" in json.loads(err)["error"]


def test_plan_diff_requires_plan_when_several(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plans": [plan_doc(1), plan_doc(2)], "errors": []})
  with patch("sys.argv", ["plan", "diff"]), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_GET_PATCH_TARGET, return_value=resp), \
       pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 1
  err = capsys.readouterr().err
  assert "diff requires --plan" in json.loads(err)["error"]
