"""The git stash-write guard wrapper (src/agents/git_stash_guard/git).

Worker.run puts the guard directory first on the PATH of every worker and
reviewer child, so these tests invoke the wrapper as a subprocess in fresh
temp repos with real git reachable behind it: stash write forms (and aliases
expanding to them, shell aliases included) return 2 with the plan's notice
byte for byte while refs/stash, its reflog and the dirty worktree stay
untouched; read forms and every other command come out byte-identical to real
git. The worker/reviewer child PATH contract is checked through the launch
harness for a work Run and a review Run.
"""

from __future__ import annotations

import asyncio
import os
import shutil
import subprocess
from pathlib import Path

import pytest
from conftest import (
    SpawningScriptedBackend,
    _adapter_with_silent_broadcast,
    create_task,
    install_backends,
    patch_instructions_content,
    result_event,
    run_git,
    stub_credentials,
)

from src.agents.worker import GIT_STASH_GUARD_DIR
from src.core import event_types as ET
from src.core.models import RunRecord

WRAPPER = GIT_STASH_GUARD_DIR / "git"

# Plan 3 v3, section 4.1: the refusal notice, byte for byte.
NOTICE = (
    b"CharlieBot worker runs refuse git stash writes: every worktree of this\n"
    b"repository shares one stash stack, so a pop can take another task's entry.\n"
    b"Set changes aside by committing them on the task branch.\n"
    b"Compare against a clean tree in a separate snapshot:\n"
    b'  git worktree add --detach "$(mktemp -d)" HEAD\n'
    b"  (remove it afterwards with: git worktree remove <dir>)\n")

# Every stash write form the guard must refuse, global options and aliases
# included (plan 3 v3, section 4.2's test list).
REFUSAL_ARGVS = [
    ["stash"],
    ["stash", "push"],
    ["stash", "-u"],
    ["stash", "pop"],
    ["stash", "apply"],
    ["stash", "drop"],
    ["stash", "clear"],
    ["-C", ".", "stash"],
    ["-c", "k=v", "stash"],
    ["-c", "alias.s=stash", "s"],
    ["-c", "alias.t=!git stash", "t"],
    ["-c", "alias.a=b", "-c", "alias.b=stash", "a"],
]

# Commands the guard must hand to real git unchanged; stdout, stderr and exit
# code come out byte-identical.
PARITY_ARGVS = [
    ["stash", "list"],
    ["stash", "show"],
    ["status"],
    ["log", "--oneline", "-1"],
    ["rev-parse", "--show-toplevel"],
    ["-c", "alias.loop=1", "-c", "alias.x=y", "-c", "alias.y=x", "x"],
    ["-c", "alias.p=!echo PFX=[$GIT_PREFIX] PWD=[$(pwd)] A=[$@]", "p", "x", "y"],
    ["-c", "alias.x=!/nonexistent-bin", "x"],
]


def _git_env(tmp_path: Path) -> dict[str, str]:
  """Isolated git config so host gitconfig cannot leak into the run."""
  env = dict(os.environ)
  env["GIT_CONFIG_GLOBAL"] = str(tmp_path / "gitconfig")
  env["GIT_CONFIG_SYSTEM"] = os.devnull
  return env


def _real_git(env: dict[str, str]) -> str:
  real = shutil.which("git", path=env["PATH"])
  assert real is not None, "real git must be on PATH for the comparison runs"
  return real


def _guard_run(repo: Path, argv: list[str], env: dict[str, str]) -> subprocess.CompletedProcess:
  """Run the wrapper with its directory first on PATH, like Worker.run does."""
  child_env = dict(env)
  child_env["PATH"] = f"{GIT_STASH_GUARD_DIR}{os.pathsep}{env['PATH']}"
  return subprocess.run([str(WRAPPER), *argv], cwd=repo, env=child_env, capture_output=True, check=False)


@pytest.fixture
def guard_repo(tmp_path: Path) -> tuple[Path, dict[str, str]]:
  """A fresh repo with one seeded stash entry and a dirty tracked file.

    The stash entry is seeded with real git so ``stash list``/``stash show``
    have content to return, and the tracked file is re-dirtied afterwards so a
    refused write is provably not the thing that kept the change.
    """
  env = _git_env(tmp_path)
  repo = tmp_path / "repo"
  repo.mkdir()
  real = _real_git(env)

  def run(*args: str) -> None:
    subprocess.run([real, "-C", str(repo), *args], env=env, capture_output=True, check=True)

  run("init", "-q", "-b", "main")
  run("config", "user.email", "t@example.com")
  run("config", "user.name", "t")
  (repo / "tracked.txt").write_text("seed\n")
  run("add", "-A")
  run("commit", "-q", "-m", "seed")
  (repo / "tracked.txt").write_text("seed\nstashed\n")
  run("stash", "push", "-q", "-m", "seeded entry")
  (repo / "tracked.txt").write_text("seed\ndirty\n")
  (repo / "sub").mkdir()
  return repo, env


def _stash_state(repo: Path, env: dict[str, str]) -> tuple[str, str]:
  """refs/stash's sha and its reflog, the state a refusal must not move."""
  ref = run_git(repo, "rev-parse", "refs/stash", env=env)
  reflog = run_git(repo, "reflog", "show", "refs/stash", env=env)
  return ref, reflog


@pytest.mark.parametrize("argv", REFUSAL_ARGVS, ids=lambda argv: " ".join(argv))
def test_stash_write_forms_are_refused(guard_repo, tmp_path, argv) -> None:
  repo, env = guard_repo
  before = _stash_state(repo, env)

  proc = _guard_run(repo, argv, env)

  assert proc.returncode == 2
  assert proc.stderr == NOTICE
  assert _stash_state(repo, env) == before
  assert "dirty" in (repo / "tracked.txt").read_text()


def test_repo_level_alias_chain_is_refused(guard_repo) -> None:
  repo, env = guard_repo
  run_git(repo, "config", "alias.r1", "r2", env=env)
  run_git(repo, "config", "alias.r2", "stash", env=env)
  before = _stash_state(repo, env)

  proc = _guard_run(repo, ["r1"], env)

  assert proc.returncode == 2
  assert proc.stderr == NOTICE
  assert _stash_state(repo, env) == before
  assert "dirty" in (repo / "tracked.txt").read_text()


def test_shell_alias_inner_stash_is_refused(guard_repo) -> None:
  """The `!` alias body runs with the guard directory still first on PATH.

    Real git would prepend its exec-path (which ships a real git) to the child
    PATH and let the inner `git stash` bypass the guard; the wrapper runs the
    body itself instead.
    """
  repo, env = guard_repo
  before = _stash_state(repo, env)

  proc = _guard_run(repo, ["-c", "alias.t=!git stash push -m inner", "t"], env)

  assert proc.returncode == 2
  assert proc.stderr == NOTICE
  assert _stash_state(repo, env) == before
  assert "dirty" in (repo / "tracked.txt").read_text()


def test_shell_alias_unexecutable_slash_body_matches_real_git(guard_repo, tmp_path) -> None:
  """A `!` body carrying a slash is exec'd directly: git's first line is
    "fatal: cannot exec" with the errno text, not the PATH lookup's
    "error: cannot run" form."""
  repo, env = guard_repo
  real = _real_git(env)
  no_exec = tmp_path / "no-exec-bin"
  no_exec.write_text("#!/bin/sh\necho hi\n")
  no_exec.chmod(0o644)
  argv = ["-c", f"alias.x=!{no_exec}", "x"]

  guarded = _guard_run(repo, argv, env)
  plain = subprocess.run([real, *argv], cwd=repo, env=env, capture_output=True, check=False)

  assert (guarded.returncode, guarded.stdout, guarded.stderr) == (plain.returncode, plain.stdout, plain.stderr)
  assert guarded.stderr.startswith(b"fatal: cannot exec '")
  assert b"Permission denied" in guarded.stderr


@pytest.mark.parametrize("argv", PARITY_ARGVS, ids=lambda argv: " ".join(argv))
def test_read_and_other_commands_match_real_git(guard_repo, argv) -> None:
  repo, env = guard_repo
  real = _real_git(env)

  guarded = _guard_run(repo, argv, env)
  plain = subprocess.run([real, *argv], cwd=repo, env=env, capture_output=True, check=False)

  assert (guarded.returncode, guarded.stdout, guarded.stderr) == (plain.returncode, plain.stdout, plain.stderr)


def test_guard_survives_its_directory_twice_on_path(guard_repo) -> None:
  """A repeated guard directory is dropped whole; the run still terminates."""
  repo, env = guard_repo
  child_env = dict(env)
  guard = str(GIT_STASH_GUARD_DIR)
  child_env["PATH"] = os.pathsep.join([guard, guard, env["PATH"]])

  proc = subprocess.run(
      [str(WRAPPER), "rev-parse", "--show-toplevel"], cwd=repo, env=child_env, capture_output=True, check=False)

  assert proc.returncode == 0
  assert proc.stdout.decode().strip() == str(repo)


def test_guard_fails_loudly_without_real_git(guard_repo) -> None:
  repo, env = guard_repo
  child_env = dict(env)
  child_env["PATH"] = str(GIT_STASH_GUARD_DIR)

  proc = subprocess.run([str(WRAPPER), "status"], cwd=repo, env=child_env, capture_output=True, check=False)

  assert proc.returncode != 0
  assert b"git-stash-guard" in proc.stderr


def test_guard_directory_holds_only_the_wrapper() -> None:
  """The whole directory rides first on PATH, so it must hold nothing else."""
  assert sorted(p.name for p in GIT_STASH_GUARD_DIR.iterdir()) == ["git"]
  assert os.access(WRAPPER, os.X_OK)


@pytest.mark.integration
@pytest.mark.asyncio
async def test_worker_and_review_runs_put_the_guard_first_on_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Both launch paths hand the backend spawn a PATH led by the guard."""
  from tests.test_task_execution import (
      build_env,
      init_repo_with_origin,
      make_pm_build,
  )

  cfg, session_mgr, tree = build_env(tmp_path, monkeypatch)
  repo, _origin = init_repo_with_origin(tmp_path)
  manager = await create_task(tree, parent=None, request_id="root")
  task_spec = {
      "goal": "## Goal\n\nadd a marker file\n",
      "repo_path": str(repo),
      "base_branch": "origin/main",
      "task_type": "implement",
      "keep_worktree": False,
  }
  worker = await create_task(tree, parent=manager.id, request_id="w", profile="worker", task=_spec(tree, task_spec))
  tree.dispatch.executor = _adapter_with_silent_broadcast(cfg, session_mgr, tree, monkeypatch)
  monkeypatch.setattr("src.agents.backends.registry.build_backend", make_pm_build("manager turn", []))
  patch_instructions_content(monkeypatch)
  stub_credentials({"charliebot": {"access_key": "op-secret"}})
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off and implement the marker file.", actor="user")

  committed: asyncio.Event = asyncio.Event()
  work_backend = SpawningScriptedBackend([result_event("implemented")], gate=committed.wait)
  review_backend = SpawningScriptedBackend([result_event("review ok")])
  install_backends(monkeypatch, [work_backend, review_backend], "src.agents.worker.build_backend")

  record = RunRecord(
      id="run-work",
      session_id=worker.id,
      kind="work",
      backend="fake",
      model="fake-model",
      repo_path=str(repo),
      base_branch="origin/main")
  await tree.runs.register_run(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker.id, "run-work")

  deadline = asyncio.get_event_loop().time() + 10
  while asyncio.get_event_loop().time() < deadline:
    run = await tree.runs.get_run(worker.id, "run-work")
    if run is not None and run.worktree_path:
      break
    await asyncio.sleep(0.05)
  else:
    pytest.fail("the work run never recorded its worktree")
  wt = Path(run.worktree_path)
  (wt / "marker.txt").write_text("implemented\n")
  run_git(wt, "add", "-A")
  run_git(wt, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "implement marker")
  committed.set()

  deadline = asyncio.get_event_loop().time() + 15
  while asyncio.get_event_loop().time() < deadline:
    runs = {r.id: r for r in tree.runs.list_run_records_sync(worker.id)}
    review = [r for r in runs.values() if r.kind == "review"]
    if review and tree.runs.terminal_outcome(tree.runs.load_events_sync(worker.id), review[0].id) is not None:
      break
    await asyncio.sleep(0.1)
  else:
    pytest.fail("the review chain never produced a terminal fact")

  first_path_entry = work_backend.env["PATH"].split(os.pathsep)[0]
  assert first_path_entry == str(GIT_STASH_GUARD_DIR)
  assert review_backend.env["PATH"].split(os.pathsep)[0] == str(GIT_STASH_GUARD_DIR)


def _spec(tree, spec: dict):
  from tests.test_task_execution import _task_spec
  return _task_spec(tree, spec)
