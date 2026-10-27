"""Build a local-origin repository for worker delegation parity."""

import os
import pathlib
import subprocess

from rust.parity.fixture import Builder

GIT_ENV = {
    "GIT_AUTHOR_NAME": "Synthetic Author",
    "GIT_AUTHOR_EMAIL": "author@example.invalid",
    "GIT_COMMITTER_NAME": "Synthetic Author",
    "GIT_COMMITTER_EMAIL": "author@example.invalid",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_DATE": "2026-01-05T09:00:00+0000",
    "GIT_COMMITTER_DATE": "2026-01-05T09:00:00+0000",
}


def _git(repo: pathlib.Path, *args: str) -> None:
  subprocess.run(["git", *args], cwd=repo, env={**os.environ, **GIT_ENV}, check=True, capture_output=True, text=True)


async def build(b: Builder) -> dict:
  workspace = b.cfg.charliebot_home.parent / "workspace"
  origin = workspace / "worker-origin.git"
  repo = workspace / "worker-repo"
  origin.mkdir(parents=True)
  subprocess.run(
      ["git", "init", "-q", "--bare", str(origin)],
      env={**os.environ, **GIT_ENV},
      check=True,
      capture_output=True,
      text=True,
  )
  subprocess.run(
      ["git", "--git-dir", str(origin), "symbolic-ref", "HEAD", "refs/heads/main"],
      env={**os.environ, **GIT_ENV},
      check=True,
      capture_output=True,
      text=True,
  )
  repo.mkdir()
  _git(repo, "init", "-q", "-b", "main")
  _git(repo, "config", "core.logAllRefUpdates", "false")
  _git(repo, "commit", "-q", "--allow-empty", "-m", "synthetic base")
  _git(repo, "remote", "add", "origin", "../worker-origin.git")
  _git(repo, "push", "-q", "-u", "origin", "main")
  _git(repo, "read-tree", "HEAD")
  return {"repo": "workspace/worker-repo"}
