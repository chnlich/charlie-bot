"""Tests for src/cli/delegate.py."""

import json
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest
import requests
from conftest import assert_cli_reject, assert_cli_reject_exit2, patched_cli_post
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


def _mock_config(tmp_path: Path):
  cfg = MagicMock()
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
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)
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

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, session="s1")) as post_mock:
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

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--task-type", task_type,
                                        session="s1")) as post_mock:
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

  with patched_cli_post(cfg, [
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

  with patch("sys.argv", [
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
  ]), pytest.raises(SystemExit) as exc_info:
    main()

  assert_cli_reject(exc_info, capsys, flag, "forbidden")


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

  with patch("sys.argv", [
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
  ]), pytest.raises(SystemExit) as exc_info:
    main()

  assert_cli_reject(exc_info, capsys, missing_flag, "required")


def test_main_rejects_relative_repo_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)

  with (
      patched_cli_post(cfg, _repo_argv("meshy-research", task_spec_file, session="s1")) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert_cli_reject(exc_info, capsys, "must be an absolute path", "meshy-research")
  post_mock.assert_not_called()


def test_main_rejects_nonexistent_repo_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)
  nonexistent = str(tmp_path / "nonexistent")

  with (
      patched_cli_post(cfg, _repo_argv(nonexistent, task_spec_file, session="s1")) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert_cli_reject(exc_info, capsys, "does not exist", nonexistent)
  post_mock.assert_not_called()


def test_main_help_lists_verify_profile(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["delegate", "--help"]), pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 0
  out = capsys.readouterr().out
  assert "verify" in out
  assert "read-only plan verifier" in out


def test_main_help_states_backend_omission_rule(capsys: pytest.CaptureFixture[str]) -> None:
  with patch("sys.argv", ["delegate", "--help"]), pytest.raises(SystemExit) as exc_info:
    main()

  assert exc_info.value.code == 0
  out = " ".join(capsys.readouterr().out.split())
  assert "Omit --backend unless the user explicitly named a backend for this delegation" in out
  assert "verify is routed to the first backends.preference entry that differs from it" in out


def test_main_posts_reviewer_context_file_as_context(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)
  reviewer_context_file = tmp_path / "reviewer_context.md"
  reviewer_context_file.write_text("review these state-machine edges")

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--reviewer-context-file",
                                        str(reviewer_context_file), session="s1")) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t3"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["context"] == "review these state-machine edges"
  assert payload["delegate_invocation"]["task_spec_file"] == str(task_spec_file)
  assert payload["delegate_invocation"]["reviewer_context_file"] == str(reviewer_context_file)


_REQUIRED_FLAG_OMISSION_CASES = [
    pytest.param("--task-spec-file", id="task-spec-file-omitted"),
    pytest.param("--keep-worktree", id="keep-worktree-omitted"),
]


@pytest.mark.parametrize("required_flag", _REQUIRED_FLAG_OMISSION_CASES)
def test_main_rejects_omitted_required_flag(
    tmp_path: Path, capsys: pytest.CaptureFixture[str], required_flag: str) -> None:
  """Omitting a required repo-skeleton flag rejects with a message naming the flag."""
  flag_values = {
      "--repo": str(tmp_path),
      "--base-branch": "main",
      "--task-spec-file": str(_write_task_spec(tmp_path)),
      "--keep-worktree": "0",
  }
  del flag_values[required_flag]
  argv = ["delegate", "--session", "s1"]
  for flag, value in flag_values.items():
    argv += [flag, value]

  with patch("sys.argv", argv), pytest.raises(SystemExit) as exc_info:
    main()

  assert_cli_reject(exc_info, capsys, required_flag)


@pytest.mark.parametrize(
    ("legacy_flag", "legacy_value"),
    [
        ("--description", "task"),
        ("--context", "review hint"),
    ],
    ids=["legacy-description", "legacy-context"],
)
def test_main_rejects_removed_legacy_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], legacy_flag: str,
    legacy_value: str) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with (
      patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, legacy_flag, legacy_value, session="s1")) as
      post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert_cli_reject(exc_info, capsys, legacy_flag)
  post_mock.assert_not_called()


def test_main_rejects_invalid_task_type(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with (
      patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--task-type", "bogus", session="s1")),
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert_cli_reject(exc_info, capsys, "--task-type")


def test_main_rejects_legacy_require_review_flag(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _mock_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  task_spec_file = _write_task_spec(tmp_path)

  with (
      patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--require-review", "0", session="s1")),
      pytest.raises(SystemExit) as exc_info,
  ):
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
      self.response.json.return_value = {"detail": "requested backend 'missing' is not in backends.options"}

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--backend", "missing",
                                        session="s1")) as post_mock:
    post_mock.side_effect = FakeRequestException()
    with pytest.raises(SystemExit) as exc_info:
      main()

  assert exc_info.value.code == 1


@pytest.mark.parametrize(
    ("file_flag", "staged_name", "with_valid_task_spec"),
    [
        pytest.param("--task-spec-file", "task_spec.md", False, id="task-spec-file"),
        pytest.param("--reviewer-context-file", "reviewer_context.md", True, id="reviewer-context-file"),
    ],
)
@pytest.mark.parametrize(
    ("file_body", "err_fragment"),
    [
        pytest.param(None, "not found", id="missing"),
        pytest.param("  \n", "empty", id="empty"),
    ],
)
def test_main_rejects_unusable_file_argument(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    file_flag: str,
    staged_name: str,
    with_valid_task_spec: bool,
    file_body: str | None,
    err_fragment: str,
) -> None:
  """An unusable required file argument rejects before posting, naming the flag and the cause."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  staged_file = tmp_path / staged_name
  if file_body is not None:
    staged_file.write_text(file_body)
  if with_valid_task_spec:
    task_spec_file = _write_task_spec(tmp_path)
    extra_argv = [file_flag, str(staged_file)]
  else:
    task_spec_file = staged_file
    extra_argv = []

  with (
      patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, *extra_argv)) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert_cli_reject_exit2(exc_info, capsys, file_flag, err_fragment)
  post_mock.assert_not_called()


def test_main_rejects_task_spec_missing_required_heading(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec().replace("## Required Behavior\n", ""))

  with (
      patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert_cli_reject_exit2(exc_info, capsys, "## Required Behavior")
  post_mock.assert_not_called()


@pytest.mark.parametrize(
    ("source_line", "err_fragment"),
    [
        ("- /definitely/not/there/task-source.md", "/definitely/not/there/task-source.md"),
        ("- relative/source.md", "absolute paths"),
        ("", "Source Files section"),
    ],
    ids=["nonexistent-absolute", "relative-entry", "empty-section"],
)
def test_main_rejects_bad_source_files_entry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], source_line: str,
    err_fragment: str) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec(source_line))

  with (
      patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    main()

  assert_cli_reject_exit2(exc_info, capsys, err_fragment)
  post_mock.assert_not_called()


def test_main_allows_source_files_none(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec("- (none)"))

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1"}
    main()

  post_mock.assert_called_once()


def test_main_accepts_existing_absolute_source_file(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  source_file = tmp_path / "source.md"
  source_file.write_text("reference")
  task_spec_file = _write_task_spec(tmp_path, _task_spec(f"- {source_file}"))

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1"}
    main()

  post_mock.assert_called_once()


@pytest.mark.parametrize(
    ("session_arg",),
    [
        (None,),
        ("abc",),
    ],
    ids=["derived-from-cwd", "explicit-flag-matching-cwd"],
)
def test_session_id_reaches_payload_from_cwd_or_flag(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    session_arg: str | None,
) -> None:
  """The resolved session id lands in the POST payload whether cwd derived it or an
  explicit --session matching the cwd supplied it."""
  cfg = _setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)

  with patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, session=session_arg)) as post_mock:
    post_mock.return_value.json.return_value = {"thread_id": "t1"}
    main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"
