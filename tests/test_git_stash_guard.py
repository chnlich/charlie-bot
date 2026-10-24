"""The git stash-write guard wrapper (src/runtime/git_stash_guard/git).

Worker.run puts the guard directory first on the PATH of every worker and
reviewer child, so these tests invoke the wrapper as a subprocess in fresh
temp repos with real git reachable behind it: direct stash write forms return
2 with the notice byte for byte while refs/stash, its reflog and the dirty
worktree stay untouched; stash read forms, alias forms and every other
command come out byte-identical to real git, because the wrapper leaves
alias resolution to git: an alias expanding to stash runs in real git. The
worker/reviewer child PATH contract is checked through the launch harness for
a work Run and a review Run.
"""

from __future__ import annotations

import asyncio
import os
import pathlib
import shutil
import subprocess

import conftest
import pytest

from src.infra import event_types as ET
from src.infra import models
from src.runtime import worker

WRAPPER = worker.GIT_STASH_GUARD_DIR / "git"

# The refusal notice, byte for byte; plan 1 v4 carries it over unchanged.
NOTICE = (
    b"CharlieBot worker runs refuse git stash writes: every worktree of this\n"
    b"repository shares one stash stack, so a pop can take another task's entry.\n"
    b"Set changes aside by committing them on the task branch.\n"
    b"Compare against a clean tree in a separate snapshot:\n"
    b'  git worktree add --detach "$(mktemp -d)" HEAD\n'
    b"  (remove it afterwards with: git worktree remove <dir>)\n")

# Every direct stash write form the guard must refuse, global options
# included (plan 1 v4); alias forms pass to real git, so none appear here.
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


def _is_guard_shim_dir(entry: str) -> bool:
  """The guard shim by Worker.run's definition of GIT_STASH_GUARD_DIR: a
  ``git_stash_guard`` directory inside a ``src/runtime`` tree. The launching
  checkout and the imported tree can differ (a worktree-run session inherits
  the launcher's PATH), so any checkout's shim directory matches."""
  resolved = pathlib.Path(entry).resolve()
  return (
      resolved.name == "git_stash_guard" and resolved.parent.name == "runtime" and resolved.parent.parent.name == "src")


def _path_without_guard_shim(raw_path: str) -> str:
  """PATH minus every guard shim directory. Sessions launched by Worker.run
  carry the shim first on PATH; the fixture seeding below must reach real git,
  while the test bodies put the shim back via _guard_run."""
  return os.pathsep.join(entry for entry in raw_path.split(os.pathsep) if not _is_guard_shim_dir(entry))


def _git_env(tmp_path: pathlib.Path) -> dict[str, str]:
  """Isolated git config so host gitconfig cannot leak into the run, and a PATH
  without the guard shim so the fixture's git calls are real git."""
  env = dict(os.environ)
  env["PATH"] = _path_without_guard_shim(env["PATH"])
  env["GIT_CONFIG_GLOBAL"] = str(tmp_path / "gitconfig")
  env["GIT_CONFIG_SYSTEM"] = os.devnull
  return env


def _real_git(env: dict[str, str]) -> str:
  real = shutil.which("git", path=env["PATH"])
  assert real is not None, "real git must be on PATH for the comparison runs"
  return real


def _guard_run(repo: pathlib.Path, argv: list[str], env: dict[str, str]) -> subprocess.CompletedProcess:
  """Run the wrapper with its directory first on PATH, like Worker.run does."""
  child_env = dict(env)
  child_env["PATH"] = f"{worker.GIT_STASH_GUARD_DIR}{os.pathsep}{env['PATH']}"
  return subprocess.run([str(WRAPPER), *argv], cwd=repo, env=child_env, capture_output=True, check=False)


@pytest.fixture
def guard_repo(tmp_path: pathlib.Path) -> tuple[pathlib.Path, dict[str, str]]:
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


def _stash_state(repo: pathlib.Path, env: dict[str, str]) -> tuple[str, str]:
  """refs/stash's sha and its reflog, the state a refusal must not move."""
  ref = conftest.run_git(repo, "rev-parse", "refs/stash", env=env)
  reflog = conftest.run_git(repo, "reflog", "show", "refs/stash", env=env)
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
  guard = str(worker.GIT_STASH_GUARD_DIR)
  child_env["PATH"] = os.pathsep.join([guard, guard, env["PATH"]])

  proc = subprocess.run(
      [str(WRAPPER), "rev-parse", "--show-toplevel"], cwd=repo, env=child_env, capture_output=True, check=False)

  assert proc.returncode == 0
  assert proc.stdout.decode().strip() == str(repo)


def test_guard_fails_loudly_without_real_git(guard_repo) -> None:
  repo, env = guard_repo
  child_env = dict(env)
  child_env["PATH"] = str(worker.GIT_STASH_GUARD_DIR)

  proc = subprocess.run([str(WRAPPER), "status"], cwd=repo, env=child_env, capture_output=True, check=False)

  assert proc.returncode != 0
  assert b"git-stash-guard" in proc.stderr


def test_guard_directory_holds_only_the_wrapper() -> None:
  """The whole directory rides first on PATH, so it must hold nothing else."""
  assert sorted(p.name for p in worker.GIT_STASH_GUARD_DIR.iterdir()) == ["git"]
  assert os.access(WRAPPER, os.X_OK)


@pytest.mark.integration
@pytest.mark.asyncio
async def test_worker_and_review_runs_put_the_guard_first_on_path(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Both launch paths hand the backend spawn a PATH led by the guard."""
  from tests import test_task_execution

  cfg, session_blocks, tree = test_task_execution.build_env(
      tmp_path, monkeypatch, backend_ids=["fake", "fake2"], pool=["fake", "fake2"])
  repo, _origin = conftest.init_repo_with_origin(tmp_path)
  manager = await conftest.create_task(tree, parent=None, request_id="root")
  task_spec = {
      "goal": "## Goal\n\nadd a marker file\n",
      "repo_path": str(repo),
      "base_branch": "origin/main",
      "task_type": "implement",
      "keep_worktree": False,
  }
  worker_node = await conftest.create_task(
      tree, parent=manager.id, request_id="w", profile="worker", task=_spec(tree, task_spec))
  tree.dispatch.executor = test_task_execution._adapter_with_silent_broadcast(cfg, session_blocks, tree, monkeypatch)
  monkeypatch.setattr(conftest.BUILD_BACKEND_PATCH_TARGET, test_task_execution.make_pm_build("manager turn", []))
  conftest.stub_credentials({"charliebot": {"access_key": "op-secret"}})
  await tree.dispatch.admit_input(
      manager.id, event_type=ET.USER, content="Take off and implement the marker file.", actor="user")

  committed: asyncio.Event = asyncio.Event()
  work_backend = test_task_execution.SpawningScriptedBackend(
      [test_task_execution.result_event("implemented")], gate=committed.wait)
  review_backend = test_task_execution.SpawningScriptedBackend([test_task_execution.result_event("review ok")])
  conftest.install_scripted_backends(
      monkeypatch, [work_backend, review_backend], conftest.WORKER_BUILD_BACKEND_PATCH_TARGET)

  record = models.RunRecord(
      id="run-work",
      session_id=worker_node.id,
      kind="work",
      backend="fake",
      model="fake-model",
      repo_path=str(repo),
      base_branch="origin/main")
  await tree.runs.register_run_locked(record, task_spec_text=task_spec["goal"])
  tree.dispatch.executor.launch(worker_node.id, "run-work")

  wt = await test_task_execution.wait_for_worktree(tree, worker_node.id, "run-work", timeout=10.0)
  (wt / "marker.txt").write_text("implemented\n")
  conftest.run_git(wt, "add", "-A")
  conftest.run_git(wt, "-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", "implement marker")
  committed.set()

  await test_task_execution.wait_for_review_terminal(tree, worker_node.id, timeout=15.0, what="the review chain")

  first_path_entry = work_backend.env["PATH"].split(os.pathsep)[0]
  assert first_path_entry == str(worker.GIT_STASH_GUARD_DIR)
  assert review_backend.env["PATH"].split(os.pathsep)[0] == str(worker.GIT_STASH_GUARD_DIR)


def _spec(tree, spec: dict):
  from tests import test_task_execution
  return test_task_execution._task_spec(tree, spec)
