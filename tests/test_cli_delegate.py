"""Tests for src/cli/delegate.py."""

import contextlib
import json
from collections.abc import Iterator
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests
from conftest import (
  CLI_COMMON_GET_CONFIG_PATCH_TARGET,
  CLI_COMMON_REQUESTS_POST_PATCH_TARGET,
)
from conftest import setup_session_cwd as _setup_session_cwd

from src.cli.delegate import main


@contextlib.contextmanager
def _patched_main(cfg: MagicMock, argv: list[str]) -> Iterator[MagicMock]:
  """Patch the externals a delegate main() call touches: sys.argv becomes argv, get_config returns
  cfg, and requests.post is a MagicMock (yielded, so tests set the response or assert no call)."""
  with patch("sys.argv", argv), \
       patch(CLI_COMMON_GET_CONFIG_PATCH_TARGET, return_value=cfg), \
       patch(CLI_COMMON_REQUESTS_POST_PATCH_TARGET) as post_mock:
    yield post_mock


def _repo_argv(repo: str, task_spec_file: Path, *extra: str, session: str | None = None) -> list[str]:
  """Argv for a repo-scoped delegate main() call: the required repo/base-branch/task-spec-file and
  keep-worktree skeleton plus the flags this test adds (extra); session stays omitted when the test
  lets cwd supply it."""
  argv = ["delegate"]
  if session is not None:
    argv += ["--session", session]
  return [
      *argv,
      "--repo",
      repo,
      "--base-branch",
      "main",
      "--task-spec-file",
      str(task_spec_file),
      *extra,
      "--keep-worktree",
      "0",
  ]


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

  with _patched_main(
      cfg, _repo_argv(str(tmp_path), task_spec_file, "--backend", "codex-o3", session="s1")) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1", "description": "do work"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "s1"
  assert payload["repo_path"] == str(tmp_path)
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"
  assert payload["description"] == task_spec
  assert payload["task_type"] == "implement"
  assert payload["delegate_invocation"] == {
      "task_type": "implement",
      "repo_path": str(tmp_path),
      "base_branch": "main",
      "task_spec_file": str(task_spec_file),
      "reviewer_context_file": None,
      "keep_worktree": False,
      "backend": "codex-o3",
  }
  assert "context" not in payload


def test_main_prints_async_wake_up_hint_to_stderr(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file, session="s1")) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1", "description": "do work"}
    main()

  captured = capsys.readouterr()
  assert "async wake-up" in captured.err
  assert json.loads(captured.out)["thread_id"] == "t1"


@pytest.mark.parametrize("task_type", ["implement", "quick-edit", "script-run"])
def test_main_task_type_lands_in_payload(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, task_type: str) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with _patched_main(
      cfg, _repo_argv(str(tmp_path), task_spec_file, "--task-type", task_type, session="s1")) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t2", "description": "task"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["task_type"] == task_type
  assert payload["delegate_invocation"]["task_type"] == task_type
  assert payload["delegate_invocation"]["repo_path"] == str(tmp_path)
  assert payload["delegate_invocation"]["base_branch"] == "main"
  assert payload["delegate_invocation"]["task_spec_file"] == str(task_spec_file)


def test_main_verify_posts_repoless_payload(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with _patched_main(
      cfg,
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
      ]) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t2", "description": "task"}
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


@pytest.mark.parametrize("flag", ["--repo", "--base-branch"])
def test_main_verify_rejects_repo_scoped_arguments(
    tmp_path: Path,
    flag: str,
    capsys: pytest.CaptureFixture[str],
) -> None:
  task_spec_file = _write_task_spec(tmp_path)
  value = str(tmp_path) if flag == "--repo" else "main"

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
    ("provide_repo", "missing_flag"),
    [
        (False, "--repo"),
        (True, "--base-branch"),
    ],
)
def test_main_repo_task_types_require_repo_and_base_branch(
    tmp_path: Path,
    task_type: str,
    provide_repo: bool,
    missing_flag: str,
    capsys: pytest.CaptureFixture[str],
) -> None:
  task_spec_file = _write_task_spec(tmp_path)
  argv_tail = ["--repo", str(tmp_path)] if provide_repo else ["--base-branch", "main"]

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


def test_main_rejects_relative_repo_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)

  with _patched_main(cfg, _repo_argv("meshy-research", task_spec_file, session="s1")) as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "must be an absolute path" in err
  assert "meshy-research" in err


def test_main_rejects_nonexistent_repo_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)
  nonexistent = str(tmp_path / "nonexistent")

  with _patched_main(cfg, _repo_argv(nonexistent, task_spec_file, session="s1")) as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code != 0
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "does not exist" in err
  assert nonexistent in err


def test_main_help_lists_verify_profile(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["delegate", "--help"]):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 0
  out = capsys.readouterr().out
  assert "verify" in out
  assert "read-only plan verifier" in out


def test_main_help_states_backend_omission_rule(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["delegate", "--help"]):
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 0
  out = " ".join(capsys.readouterr().out.split())
  assert "Omit --backend unless the user explicitly named a backend for this delegation" in out
  assert "verify is routed to the first model_preference entry that differs from it" in out


def test_main_posts_reviewer_context_file_as_context(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)
  reviewer_context_file = tmp_path / "reviewer_context.md"
  reviewer_context_file.write_text("review these state-machine edges")

  with _patched_main(
      cfg,
      _repo_argv(
          str(tmp_path), task_spec_file, "--reviewer-context-file", str(reviewer_context_file),
          session="s1")) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t3"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["context"] == "review these state-machine edges"
  assert payload["delegate_invocation"]["task_spec_file"] == str(task_spec_file)
  assert payload["delegate_invocation"]["reviewer_context_file"] == str(reviewer_context_file)


def test_main_requires_task_spec_file(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", [
      "delegate",
      "--session",
      "s1",
      "--repo",
      str(tmp_path),
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

  with _patched_main(
      cfg, _repo_argv(str(tmp_path), task_spec_file, "--description", "task", session="s1")) as post_mock:
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

  with _patched_main(
      cfg, _repo_argv(str(tmp_path), task_spec_file, "--context", "review hint", session="s1")) as post_mock:
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

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file, "--task-type", "bogus", session="s1")):
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

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file, "--require-review", "0", session="s1")):
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

  with _patched_main(
      cfg, _repo_argv(str(tmp_path), task_spec_file, "--backend", "missing", session="s1")) as post_mock:
    post_mock.side_effect = FakeRequestException()
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
      str(tmp_path),
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

  with _patched_main(cfg, _repo_argv(str(tmp_path), missing)) as post_mock:
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

  with _patched_main(cfg, _repo_argv(str(tmp_path), empty)) as post_mock:
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

  with _patched_main(
      cfg, _repo_argv(str(tmp_path), task_spec_file, "--reviewer-context-file", str(missing))) as post_mock:
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

  with _patched_main(
      cfg, _repo_argv(str(tmp_path), task_spec_file, "--reviewer-context-file", str(empty))) as post_mock:
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

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
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

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
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

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
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

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  err = capsys.readouterr().err
  assert "Source Files section" in err


def test_main_allows_source_files_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec("- (none)"))

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1"}
    main()

  post_mock.assert_called_once()


def test_main_accepts_existing_absolute_source_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  source_file = tmp_path / "source.md"
  source_file.write_text("reference")
  task_spec_file = _write_task_spec(tmp_path, _task_spec(f"- {source_file}"))

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1"}
    main()

  post_mock.assert_called_once()


def test_session_auto_derived_from_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"


def test_session_matches_cwd(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)

  with _patched_main(cfg, _repo_argv(str(tmp_path), task_spec_file, session="abc")) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"
