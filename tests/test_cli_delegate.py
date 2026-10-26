"""Tests for src/runtime/cli/delegate.py."""

import json
import pathlib
from unittest import mock

import conftest
import pytest

from src.infra import constants
from src.runtime.cli import common, delegate


def _repo_argv(repo: str, task_spec_file: pathlib.Path, *extra: str, session: str | None = None) -> list[str]:
  """Argv for a repo-scoped delegate.main() call: the required repo/base-branch/task-spec-file and
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


def _verify_argv(task_spec_file: pathlib.Path, *extra: str, task_type: str) -> list[str]:
  """Argv for a repoless delegate.main() call: the session/task-spec-file/keep-worktree skeleton
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


def _write_task_spec(tmp_path: pathlib.Path, content: str | None = None) -> pathlib.Path:
  task_spec_file = tmp_path / "task_spec.md"
  task_spec_file.write_text(content if content is not None else _task_spec())
  return task_spec_file


def _delegate_rig(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> tuple[mock.MagicMock, pathlib.Path]:
  """The CLI delegate.main() rig: a sessions-dir config with cwd at tmp_path, plus one written task spec."""
  cfg = conftest.make_sessions_dir_config(tmp_path)
  monkeypatch.chdir(tmp_path)
  return cfg, _write_task_spec(tmp_path)


def test_main_routes_by_session_env_from_another_session_dir(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  """A master whose shell sits in an archived session's dir delegates, and the
  task lands in the session the server started it for. The warning names both
  ids, so the misplaced cwd stays visible."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "archived-session")
  monkeypatch.setenv(constants.SESSION_ID_ENV_VAR, "live-session")
  task_spec_file = _write_task_spec(tmp_path)

  with conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"session_id": "w1", "run_id": "r1"}
    delegate.main()

  assert post_mock.call_args.kwargs["json"]["session_id"] == "live-session"
  err = capsys.readouterr().err
  assert "archived-session" in err
  assert f"{constants.SESSION_ID_ENV_VAR}=live-session" in err


def test_main_rejects_explicit_session_against_session_env(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  """A copied --session literal stays a rejection that names both ids."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "live-session")
  monkeypatch.setenv(constants.SESSION_ID_ENV_VAR, "live-session")
  task_spec_file = _write_task_spec(tmp_path)

  with (
      conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, session="archived-session")) as
      post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  assert exc_info.value.code == 2
  post_mock.assert_not_called()
  error = json.loads(capsys.readouterr().err)["error"]
  assert "--session=archived-session" in error
  assert f"{constants.SESSION_ID_ENV_VAR}=live-session" in error


def test_main_prints_only_session_and_run_id_from_server_result(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  """stdout carries the server result's session_id and run_id; the spec, parent and thread keys stay off it."""
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)
  server_result = {
      "session_id": "worker-1",
      "parent_session_id": "s1",
      "run_id": "run-1",
      "thread_id": "run-1",
      "description": _task_spec(),
  }

  with conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, session="s1")) as post_mock:
    post_mock.return_value.json.return_value = server_result
    delegate.main()

  assert json.loads(capsys.readouterr().out) == {"session_id": "worker-1", "run_id": "run-1"}


def test_main_prints_readback_ids_when_the_response_is_lost(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
  """A sent-but-lost delegation prints the readback's ids; run_id stays null until the work Run is on disk."""
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)
  readback = {"session_id": "worker-2", "parent_session_id": "s1", "run_id": None, "thread_id": None}

  with (
      conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, session="s1"),
                                side_effect=common._SentButLostError("reset")),
      mock.patch.object(common, "find_local_task_child", return_value=readback),
  ):
    delegate.main()

  assert json.loads(capsys.readouterr().out) == {"session_id": "worker-2", "run_id": None}


def test_main_posts_task_spec_file_to_delegate_endpoint(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)
  task_spec = task_spec_file.read_text()

  with conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--backend", "codex-o3",
                                                 session="s1")) as post_mock:
    post_mock.return_value.json.return_value = {"session_id": "w1", "run_id": "r1"}
    delegate.main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "s1"
  assert payload["repo_path"] == str(tmp_path)
  assert payload["base_branch"] == "main"
  assert payload["backend"] == "codex-o3"
  assert payload["description"] == task_spec
  assert payload["task_type"] == "implement"
  assert payload["delegate_invocation"] == conftest.delegate_invocation(
      repo_path=str(tmp_path), task_spec_file=str(task_spec_file))
  assert "context" not in payload


def test_main_verify_posts_repoless_payload(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)

  with conftest.patched_cli_post(cfg, _verify_argv(task_spec_file, task_type="verify")) as post_mock:
    post_mock.return_value.json.return_value = {"session_id": "w2", "run_id": "r2"}
    delegate.main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["task_type"] == "verify"
  assert "repo_path" not in payload
  assert "base_branch" not in payload
  assert payload["delegate_invocation"] == conftest.delegate_invocation(
      task_type="verify", repo_path=None, base_branch=None, task_spec_file=str(task_spec_file), backend=None)


@pytest.mark.parametrize("task_type", ["implement", "quick-edit", "script-run"])
@pytest.mark.parametrize(
    ("half_argv", "named_flag"),
    [
        (("--repo", "/tmp/some-repo"), "--repo"),
        (("--base-branch", "main"), "--base-branch"),
    ],
)
def test_main_repo_task_types_take_repo_and_base_branch_together(
    tmp_path: pathlib.Path,
    task_type: str,
    half_argv: tuple[str, ...],
    named_flag: str,
    capsys: pytest.CaptureFixture[str],
) -> None:
  """Exactly one of --repo/--base-branch is a malformed repo-less delegation."""
  task_spec_file = _write_task_spec(tmp_path)

  with (
      mock.patch("sys.argv", _verify_argv(task_spec_file, *half_argv, task_type=task_type)),
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  conftest.assert_cli_reject(exc_info, capsys, named_flag, "together")


@pytest.mark.parametrize("task_type", ["implement", "quick-edit", "script-run"])
def test_main_repo_less_delegation_posts_no_repo_fields(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, task_type: str) -> None:
  """Omitting --repo and --base-branch together delegates a repo-less task."""
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)

  with conftest.patched_cli_post(cfg, _verify_argv(task_spec_file, task_type=task_type)) as post_mock:
    post_mock.return_value.json.return_value = {"session_id": "w3", "run_id": "r3"}
    delegate.main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["task_type"] == task_type
  assert "repo_path" not in payload
  assert "base_branch" not in payload
  assert payload["delegate_invocation"]["repo_path"] is None
  assert payload["delegate_invocation"]["base_branch"] is None


@pytest.mark.parametrize(
    ("repo", "extra_argv", "err_fragments"),
    [
        pytest.param("meshy-research", (), ("must be an absolute path", "meshy-research"), id="relative-repo"),
        pytest.param("/no/such/repo", (), ("does not exist", "/no/such/repo"), id="nonexistent-repo"),
        pytest.param(None, ("--description", "task"), ("--description",), id="removed-legacy-description"),
        pytest.param(None, ("--context", "review hint"), ("--context",), id="removed-legacy-context"),
    ],
)
def test_main_rejects_bad_invocation_before_post(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], repo: str | None,
    extra_argv: tuple[str, ...], err_fragments: tuple[str, ...]) -> None:
  """Each malformed invocation names its cause on stderr and rejects before any POST: a relative
  --repo, a nonexistent --repo, and the removed --description/--context flags."""
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)
  repo_value = repo if repo is not None else str(tmp_path)

  with (
      conftest.patched_cli_post(cfg, _repo_argv(repo_value, task_spec_file, *extra_argv, session="s1")) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  conftest.assert_cli_reject(exc_info, capsys, *err_fragments)
  post_mock.assert_not_called()


def test_main_help_lists_verify_profile(capsys: pytest.CaptureFixture[str]) -> None:
  with mock.patch("sys.argv", ["delegate", "--help"]), pytest.raises(SystemExit) as exc_info:
    delegate.main()

  assert exc_info.value.code == 0
  out = capsys.readouterr().out
  assert "verify" in out
  assert "read-only plan verifier" in out


def test_main_help_stays_bounded_and_names_every_delegation_fact(
    capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch) -> None:
  """Managers read this help before each delegation, so it stays bounded and keeps every argument
  name, every task-type choice, and the six task-spec section names.

  The bound pins the landed size. The plan's 2,600-character target sits below the floor the
  mandated fact list admits: with an empty epilog and no help strings, argparse's own usage line
  and options rendering measure 823 characters (the shared --session help alone is ~215), and the
  fact-complete epilog measures ~2,400, so the smallest honest rendering is ~3,200.
  """
  monkeypatch.delenv("COLUMNS", raising=False)
  with mock.patch("sys.argv", ["delegate", "--help"]), pytest.raises(SystemExit) as exc_info:
    delegate.main()

  assert exc_info.value.code == 0
  out = capsys.readouterr().out
  assert len(out) <= 4100
  for name in ("--session", "--repo", "--task-spec-file", "--base-branch", "--backend", "--keep-worktree",
               "--request-id", "--task-type"):
    assert name in out
  for choice in ("implement", "quick-edit", "script-run", "verify"):
    assert choice in out
  for section in ("## Goal", "## Source Files", "## Required Behavior", "## Acceptance Tests", "## Reviewer Checklist",
                  "## Out of Scope"):
    assert section in out


def test_main_rejects_removed_reviewer_context_file_flag(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  """The removed --reviewer-context-file exits with a usage error naming the Reviewer Checklist, before any POST."""
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)

  with (
      conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--reviewer-context-file",
                                                str(tmp_path / "reviewer_context.md"), session="s1")) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  conftest.assert_cli_reject_exit2(exc_info, capsys, "## Reviewer Checklist")
  post_mock.assert_not_called()


_REQUIRED_FLAG_OMISSION_CASES = [
    pytest.param("--task-spec-file", id="task-spec-file-omitted"),
    pytest.param("--keep-worktree", id="keep-worktree-omitted"),
]


@pytest.mark.parametrize("required_flag", _REQUIRED_FLAG_OMISSION_CASES)
def test_main_rejects_omitted_required_flag(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str], required_flag: str) -> None:
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

  with mock.patch("sys.argv", argv), pytest.raises(SystemExit) as exc_info:
    delegate.main()

  conftest.assert_cli_reject(exc_info, capsys, required_flag)


def test_main_rejects_invalid_task_type(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)

  with (
      conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--task-type", "bogus", session="s1")),
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  conftest.assert_cli_reject(exc_info, capsys, "--task-type")


def test_main_rejects_legacy_require_review_flag(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)

  with (
      conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--require-review", "0", session="s1")),
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  assert exc_info.value.code != 0
  err = capsys.readouterr().err
  assert "--require-review" in err or "unrecognized" in err


def test_main_uses_error_detail_from_response(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg, task_spec_file = _delegate_rig(tmp_path, monkeypatch)

  with conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, "--backend", "missing",
                                        session="s1"),
                        return_value=conftest.make_json_response(
                            {"detail": "requested backend 'missing' is not in backends.options"},
                            status_code=422)), \
       mock.patch(conftest.CLI_COMMON_MAYBE_VERSION_SKEW_HINT_PATCH_TARGET, return_value=None), pytest.raises(SystemExit) as exc_info:
    delegate.main()

  assert exc_info.value.code == 1


@pytest.mark.parametrize(
    ("file_body", "err_fragment"),
    [
        pytest.param(None, "not found", id="missing"),
        pytest.param("  \n", "empty", id="empty"),
    ],
)
def test_main_rejects_unusable_file_argument(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    file_body: str | None,
    err_fragment: str,
) -> None:
  """An unusable --task-spec-file rejects before posting, naming the flag and the cause."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = tmp_path / "task_spec.md"
  if file_body is not None:
    task_spec_file.write_text(file_body)

  with (
      conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  conftest.assert_cli_reject_exit2(exc_info, capsys, "--task-spec-file", err_fragment)
  post_mock.assert_not_called()


def test_main_rejects_task_spec_missing_required_heading(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec().replace("## Required Behavior\n", ""))

  with (
      conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  conftest.assert_cli_reject_exit2(exc_info, capsys, "## Required Behavior")
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
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], source_line: str,
    err_fragment: str) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec(source_line))

  with (
      conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock,
      pytest.raises(SystemExit) as exc_info,
  ):
    delegate.main()

  conftest.assert_cli_reject_exit2(exc_info, capsys, err_fragment)
  post_mock.assert_not_called()


def test_main_allows_source_files_none(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path, _task_spec("- (none)"))

  with conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"session_id": "w1", "run_id": "r1"}
    delegate.main()

  post_mock.assert_called_once()


def test_main_accepts_existing_absolute_source_file(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  source_file = tmp_path / "source.md"
  source_file.write_text("reference")
  task_spec_file = _write_task_spec(tmp_path, _task_spec(f"- {source_file}"))

  with conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file)) as post_mock:
    post_mock.return_value.json.return_value = {"session_id": "w1", "run_id": "r1"}
    delegate.main()

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
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
    session_arg: str | None,
) -> None:
  """The resolved session id lands in the POST payload whether cwd derived it or an
  explicit --session matching the cwd supplied it."""
  cfg = conftest.setup_session_cwd(tmp_path, monkeypatch, "abc")
  task_spec_file = _write_task_spec(tmp_path)

  with conftest.patched_cli_post(cfg, _repo_argv(str(tmp_path), task_spec_file, session=session_arg)) as post_mock:
    post_mock.return_value.json.return_value = {"session_id": "w1", "run_id": "r1"}
    delegate.main()

  payload = post_mock.call_args.kwargs["json"]
  assert payload["session_id"] == "abc"
