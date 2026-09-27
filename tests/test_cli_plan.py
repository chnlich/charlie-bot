"""Tests for src/cli/plan.py — argument validation, session resolution, stdout/stderr shape."""

import json
from collections.abc import Iterator
from contextlib import AbstractContextManager, contextmanager
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from conftest import (
    CLI_COMMON_GET_CONFIG_PATCH_TARGET,
    CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET,
    CLI_COMMON_TRANSPORT_GET_PATCH_TARGET,
    CLI_COMMON_TRANSPORT_POST_PATCH_TARGET,
    _patched_cli_transport,
    make_json_response,
    patched_cli_post,
    plan_doc,
    plan_page_html,
    plan_version_v1,
    write_plan_artifact,
)
from conftest import setup_session_cwd as _setup_session_cwd

from src.cli.plan import main
from src.core import plan_diff


@contextmanager
def patched_cli_get(cfg: object, argv: list[str], **get_kw: object) -> Iterator[MagicMock]:
  """_patched_cli_transport with _request_get as the patched verb (the GET-only commands, e.g.
  plan list/diff)."""
  yield from _patched_cli_transport(CLI_COMMON_TRANSPORT_GET_PATCH_TARGET, cfg, argv, **get_kw)


def _present_post(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *extra_args: str,
) -> AbstractContextManager[MagicMock]:
  """Run `plan present --file artifacts/plan_01.html --title P1 [*extra_args]` under the patched POST.

  The POST returns the awaiting-approval plan-1 response; the context manager yields the post mock.
  """
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 1, "v": 1, "state": "awaiting approval"})
  argv = ["plan", "present", "--file", "artifacts/plan_01.html", "--title", "P1", *extra_args]
  return patched_cli_post(cfg, argv, return_value=resp)


def test_plan_present_posts_to_present_endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  with _present_post(tmp_path, monkeypatch) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"
  assert payload["file"] == "artifacts/plan_01.html"
  assert "verify_thread" not in payload
  assert payload["title"] == "P1"
  assert payload["base_repo"] is None
  assert payload["base_branch"] is None
  assert payload["base_sha"] is None


def test_plan_approve_posts_plan_id(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 1, "v": 1, "state": "approved"})
  with patched_cli_post(cfg, ["plan", "approve", "--plan", "1"], return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload == {"session_id": "abc", "plan_id": 1}

  out = capsys.readouterr().out
  assert "reminder" not in json.loads(out)


@pytest.mark.parametrize("close_as", ["superseded", "completed"])
def test_plan_close_posts(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, close_as: str) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  resp = make_json_response({"plan": 1, "state": close_as})
  with patched_cli_post(cfg, [
      "plan",
      "close",
      "--plan",
      "1",
      "--as",
      close_as,
  ], return_value=resp) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload == {"session_id": "abc", "plan_id": 1, "close_as": close_as}


def test_plan_server_rejection_exits_nonzero_with_detail_on_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")

  with (
      patched_cli_post(cfg, [
          "plan",
          "present",
          "--file",
          "artifacts/missing.html",
          "--title",
          "P1",
      ], return_value=make_json_response(
          {"detail": "file 'artifacts/missing.html' not found inside the session directory"}, status_code=422)),
      patch(CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET, return_value=None),
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
  with patched_cli_get(cfg, ["plan", "list"], return_value=resp) as get_mock:
    main()

  assert "/api/sessions/abc/plans" in get_mock.call_args.args[0]


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
       patch(CLI_COMMON_TRANSPORT_GET_PATCH_TARGET, return_value=resp) as get_mock, \
       patch(CLI_COMMON_TRANSPORT_POST_PATCH_TARGET) as post_mock:
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
