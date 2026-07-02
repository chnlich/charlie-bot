"""Tests for src/cli/delegate.py."""

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests

from src.cli.delegate import main


def _mock_config(tmp_path: Path):
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "fake_sessions"
  cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
  return cfg


def _task_spec(source_line: str = "- (none)") -> str:
  return (
      "## Goal\n"
      "Do work.\n\n"
      "## Source Files\n"
      f"{source_line}\n\n"
      "## Required Behavior\n"
      "Implement the requested behavior.\n\n"
      "## Acceptance Tests\n"
      "Run focused tests.\n\n"
      "## Reviewer Checklist\n"
      "Check the contract.\n\n"
      "## Out of Scope\n"
      "Do not change unrelated files.\n")


def _write_task_spec(tmp_path: Path, content: str | None = None) -> Path:
  task_spec_file = tmp_path / "task_spec.md"
  task_spec_file.write_text(content if content is not None else _task_spec())
  return task_spec_file


def test_main_posts_task_spec_file_to_delegate_endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)
  task_spec = task_spec_file.read_text()
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t1", "description": "do work"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--backend",
          "codex-o3",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "s1"
  assert payload["repo_path"] == "/tmp/repo"
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"
  assert payload["description"] == task_spec
  assert payload["task_type"] == "implement"
  assert payload["delegate_invocation"] == {
      "task_type": "implement",
      "repo_path": "/tmp/repo",
      "base_branch": "main",
      "task_spec_file": str(task_spec_file),
      "reviewer_context_file": None,
      "keep_worktree": False,
      "backend": "codex-o3",
  }
  assert "context" not in payload


@pytest.mark.parametrize("task_type", ["implement", "quick-edit", "script-run"])
def test_main_task_type_lands_in_payload(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, task_type: str) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t2", "description": "task"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
          "--task-type",
          task_type,
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["task_type"] == task_type
  assert payload["delegate_invocation"]["task_type"] == task_type
  assert payload["delegate_invocation"]["repo_path"] == "/tmp/repo"
  assert payload["delegate_invocation"]["base_branch"] == "main"
  assert payload["delegate_invocation"]["task_spec_file"] == str(task_spec_file)


def test_main_verify_posts_repoless_payload(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t2", "description": "task"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
          "--task-type",
          "verify",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["task_type"] == "verify"
  assert "repo_path" not in payload
  assert "base_branch" not in payload
  assert payload["delegate_invocation"] == {
      "task_type": "verify",
      "repo_path": None,
      "base_branch": None,
      "task_spec_file": str(task_spec_file),
      "reviewer_context_file": None,
      "keep_worktree": False,
      "backend": None,
  }


@pytest.mark.parametrize(("flag", "value"), [("--repo", "/tmp/repo"), ("--base-branch", "main")])
def test_main_verify_rejects_repo_scoped_arguments(
    tmp_path: Path,
    flag: str,
    value: str,
    capsys: pytest.CaptureFixture[str],
) -> None:
  task_spec_file = _write_task_spec(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
          "--task-type",
          "verify",
          flag,
          value,
      ]):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert flag in err
  assert "forbidden" in err


@pytest.mark.parametrize("task_type", ["implement", "quick-edit", "script-run"])
@pytest.mark.parametrize(
    ("argv_tail", "missing_flag"),
    [
        (["--base-branch", "main"], "--repo"),
        (["--repo", "/tmp/repo"], "--base-branch"),
    ],
)
def test_main_repo_task_types_require_repo_and_base_branch(
    tmp_path: Path,
    task_type: str,
    argv_tail: list[str],
    missing_flag: str,
    capsys: pytest.CaptureFixture[str],
) -> None:
  task_spec_file = _write_task_spec(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
          "--task-type",
          task_type,
          *argv_tail,
      ]):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert missing_flag in err
  assert "required" in err


def test_main_help_lists_verify_profile(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["delegate", "--help"]):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 0
  out = capsys.readouterr().out
  assert "verify" in out
  assert "read-only plan verifier" in out


def test_main_posts_reviewer_context_file_as_context(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)
  reviewer_context_file = tmp_path / "reviewer_context.md"
  reviewer_context_file.write_text("review these state-machine edges")
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t3"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--reviewer-context-file",
          str(reviewer_context_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["context"] == "review these state-machine edges"
  assert payload["delegate_invocation"]["task_spec_file"] == str(task_spec_file)
  assert payload["delegate_invocation"]["reviewer_context_file"] == str(reviewer_context_file)


def test_main_requires_task_spec_file(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", [
      "delegate",
      "--session",
      "s1",
      "--repo",
      "/tmp/repo",
      "--base-branch",
      "main",
      "--keep-worktree",
      "0",
  ]):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert "--task-spec-file" in err


def test_main_rejects_legacy_description_argparse(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--description",
          "task",
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "--description" in err


def test_main_rejects_legacy_context_argparse(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--context",
          "review hint",
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "--context" in err


def test_main_rejects_invalid_task_type(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
          "--task-type",
          "bogus",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert "--task-type" in err


def test_main_rejects_legacy_require_review_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
          "--require-review",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert "--require-review" in err or "unrecognized" in err


def test_main_uses_error_detail_from_response(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  class FakeRequestException(requests.RequestException):

    def __init__(self) -> None:
      super().__init__("bad request")
      self.response = MagicMock()
      self.response.json.return_value = {"detail": "requested backend 'missing' is not in backend_options"}

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "s1",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--backend",
          "missing",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", side_effect=FakeRequestException()):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 1


def test_main_requires_keep_worktree_flag(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  task_spec_file = _write_task_spec(tmp_path)

  with patch("sys.argv", [
      "delegate",
      "--session",
      "s1",
      "--repo",
      "/tmp/repo",
      "--base-branch",
      "main",
      "--task-spec-file",
      str(task_spec_file),
  ]):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert "--keep-worktree" in err


def test_main_rejects_missing_task_spec_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  missing = tmp_path / "missing.md"

  with patch(
      "sys.argv",
      ["delegate", "--repo", "/tmp/repo", "--base-branch", "main", "--task-spec-file", str(missing),
       "--keep-worktree", "0"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "task-spec-file" in err and "not found" in err


def test_main_rejects_empty_task_spec_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  empty = tmp_path / "empty.md"
  empty.write_text("  \n")

  with patch(
      "sys.argv",
      ["delegate", "--repo", "/tmp/repo", "--base-branch", "main", "--task-spec-file", str(empty),
       "--keep-worktree", "0"]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "task-spec-file" in err and "empty" in err


def test_main_rejects_missing_reviewer_context_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)
  missing = tmp_path / "missing_context.md"

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--reviewer-context-file",
          str(missing),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "reviewer-context-file" in err and "not found" in err


def test_main_rejects_empty_reviewer_context_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)
  empty = tmp_path / "empty_context.md"
  empty.write_text("  \n")

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--reviewer-context-file",
          str(empty),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "reviewer-context-file" in err and "empty" in err


def test_main_rejects_task_spec_missing_required_heading(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec().replace("## Required Behavior\n", ""))

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "## Required Behavior" in err


def test_main_rejects_nonexistent_absolute_source_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec("- /definitely/not/there/task-source.md"))

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "/definitely/not/there/task-source.md" in err


def test_main_rejects_relative_source_file_entry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec("- relative/source.md"))

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "absolute paths" in err


def test_main_rejects_empty_source_files_section(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec(""))

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post") as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "Source Files section" in err


def test_main_allows_source_files_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec("- (none)"))
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t1"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  post_mock.assert_called_once()


def test_main_accepts_existing_absolute_source_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  source_file = tmp_path / "source.md"
  source_file.write_text("reference")
  task_spec_file = _write_task_spec(tmp_path, _task_spec(f"- {source_file}"))
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t1"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  post_mock.assert_called_once()


def _setup_session_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, sid: str) -> MagicMock:
  """Build a session dir tree at <tmp_path>/sessions/<sid> and chdir into it."""
  cfg = MagicMock()
  cfg.server_port = 9443
  cfg.sessions_dir = tmp_path / "sessions"
  session_dir = cfg.sessions_dir / sid
  session_dir.mkdir(parents=True, exist_ok=True)
  monkeypatch.chdir(session_dir)
  return cfg


def test_session_auto_derived_from_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t1"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"


def test_session_matches_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)
  resp_mock = MagicMock()
  resp_mock.json.return_value = {"thread_id": "t1"}
  resp_mock.raise_for_status = MagicMock()

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "abc",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg), \
       patch("src.cli.common.requests.post", return_value=resp_mock) as post_mock:
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"


def test_session_mismatch_with_cwd_rejected(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--session",
          "xyz",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
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
  task_spec_file = _write_task_spec(tmp_path)

  with patch(
      "sys.argv",
      [
          "delegate",
          "--repo",
          "/tmp/repo",
          "--base-branch",
          "main",
          "--task-spec-file",
          str(task_spec_file),
          "--keep-worktree",
          "0",
      ]), \
       patch("src.cli.common.get_config", return_value=cfg):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "session dir" in err or "--session required" in err
