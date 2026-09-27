"""Tests for src/cli/delegate.py."""

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
from conftest import (
    assert_cli_reject,
    delegate_invocation,
    make_sessions_dir_config,
    patched_cli_post,
)
from conftest import setup_session_cwd as _setup_session_cwd

from src.cli.delegate import main


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


def _verify_argv(task_spec_file: Path, *extra: str, task_type: str) -> list[str]:
  """Argv for a repoless delegate main() call: the session/task-spec-file/keep-worktree skeleton
  plus the flags this test adds (extra); task_type stays explicit because the parametrized sites
  pass non-verify types."""
  return [
      "delegate",
      "--session",
      "s1",
      "--task-spec-file",
      str(task_spec_file),
      "--keep-worktree",
      "0",
      "--task-type",
      task_type,
      *extra,
  ]


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


def _delegate_rig(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[MagicMock, Path]:
  """The CLI main() rig: a sessions-dir config with cwd at tmp_path, plus one written task spec."""
  cfg = make_sessions_dir_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  return cfg, _write_task_spec(tmp_path)


def test_main_routes_by_session_env_from_another_session_dir(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  """A master whose shell sits in an archived session's dir delegates, and the
  task lands in the session the server started it for. The warning names both
  ids, so the misplaced cwd stays visible."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "archived-session")
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "live-session")
  task_spec_file = _write_task_spec(tmp_path)

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1", "description": "do work"}
    main()

  assert post_mock.call_args.kwargs["json"]["session_id"] == "live-session"
  err = capsys.readouterr().err
  assert "archived-session" in err
  assert "CHARLIEBOT_SESSION_ID=live-session" in err


def test_main_rejects_explicit_session_against_session_env(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  """A copied --session literal stays a rejection that names both ids."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "live-session")
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "live-session")
  task_spec_file = _write_task_spec(tmp_path)

  with (
      patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, session="archived-session")) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  error = json.loads(capsys.readouterr().err)["error"]
  assert "--session=archived-session" in error
  assert "CHARLIEBOT_SESSION_ID=live-session" in error


def test_main_posts_task_spec_file_to_delegate_endpoint(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)
  task_spec = task_spec_file.read_text()

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--backend", "codex-o3",
                                        session="s1")) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1", "description": "do work"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "s1"
  assert payload["repo_path"] == str(tmp_path)
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"
  assert payload["description"] == task_spec
  assert payload["task_type"] == "implement"
  assert payload["delegate_invocation"] == delegate_invocation(
      repo_path=str(tmp_path), task_spec_file=str(task_spec_file))
  assert "context" not in payload


def test_main_verify_posts_repoless_payload(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)

  with patched_cli_post(cfg, _verify_argv(task_spec_file, task_type="verify")) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t2", "description": "task"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["task_type"] == "verify"
  assert "repo_path" not in payload
  assert "base_branch" not in payload
  assert payload["delegate_invocation"] == delegate_invocation(
      task_type="verify", repo_path=None, base_branch=None, task_spec_file=str(task_spec_file), backend=None)


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

  with (
      patch("sys.argv", _verify_argv(task_spec_file, *argv_tail, task_type=task_type)),
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert_cli_reject(exc_info, capsys, missing_flag, "required")
