"""Write the repositories and the code-server stand-in the diff-viewer scenario reads.

The repository has a main and a feature branch. The feature branch modifies, adds, deletes, renames, rewrites a binary
file and rewrites a CRLF file; main gains one file after the branch point, so two-dot and three-dot ranges differ. Beside
it sit a plain directory, a stray repository outside the workspace, and a code-server stand-in that exits at once.
"""

import os
import pathlib
import subprocess

from rust.parity.fixture import Builder

# A fixed identity and fixed dates keep every commit SHA the same across builds and across the two servers. The global
# and system git configs are off, so the host's settings cannot change how the commits are written.
GIT_ENV = {
    "GIT_AUTHOR_NAME": "Synthetic Author",
    "GIT_AUTHOR_EMAIL": "author@example.invalid",
    "GIT_COMMITTER_NAME": "Synthetic Author",
    "GIT_COMMITTER_EMAIL": "author@example.invalid",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}
BASE_TIME = "2026-01-05T09:00:00+0000"
FEATURE_TIME = "2026-01-05T10:00:00+0000"
MAIN_TIME = "2026-01-05T11:00:00+0000"
# The stand-in exits with status 3 before it listens, and nothing listens on port 1.
CODE_SERVER_CONFIG = "bind-addr: 127.0.0.1:1\n"


def _git(repo: pathlib.Path, *args: str, when: str = BASE_TIME) -> None:
  # Every git call carries a fixed date, so the reflog entries that checkout writes match across builds.
  env = {**os.environ, **GIT_ENV, "GIT_AUTHOR_DATE": when, "GIT_COMMITTER_DATE": when}
  subprocess.run(["git", *args], cwd=repo, env=env, check=True, capture_output=True, text=True)


def _commit(repo: pathlib.Path, message: str, when: str) -> None:
  _git(repo, "add", "-A")
  _git(repo, "commit", "-q", "-m", message, when=when)


def _reset_index(repo: pathlib.Path) -> None:
  # The index caches each file's inode and stat times, which differ between the two homes. read-tree rebuilds it from
  # HEAD with those fields zeroed, so its bytes match across builds.
  _git(repo, "read-tree", "HEAD")


def _build_repo(repo: pathlib.Path) -> None:
  repo.mkdir(parents=True)
  _git(repo, "init", "-q", "-b", "main")
  (repo / "keep.txt").write_text("line1\nline2\nline3\n", encoding="utf-8")
  (repo / "torename.txt").write_text("old content\nsecond\n", encoding="utf-8")
  (repo / "todelete.txt").write_text("to be deleted\n", encoding="utf-8")
  (repo / "blob.bin").write_bytes(b"\x00binary v1\x00" + bytes(range(64)))
  (repo / "crlf.txt").write_bytes(b"first line\r\nsecond line\r\n")
  _commit(repo, "base", BASE_TIME)

  _git(repo, "checkout", "-q", "-b", "feature")
  (repo / "keep.txt").write_text("line1\nline2 changed\nline3\nline4\n", encoding="utf-8")
  _git(repo, "mv", "torename.txt", "renamed.txt")
  (repo / "renamed.txt").write_text("old content\nsecond\nthird\n", encoding="utf-8")
  _git(repo, "rm", "-q", "todelete.txt")
  (repo / "added.txt").write_text("brand new\nfile\n", encoding="utf-8")
  (repo / "blob.bin").write_bytes(b"\x00binary v2\x00" + bytes(range(65)))
  (repo / "crlf.txt").write_bytes(b"first line\r\nsecond line changed\r\n")
  _commit(repo, "feature", FEATURE_TIME)

  _git(repo, "checkout", "-q", "main")
  (repo / "main-only.txt").write_text("landed on main after the branch point\n", encoding="utf-8")
  _commit(repo, "main only", MAIN_TIME)
  _reset_index(repo)


def _build_stray(repo: pathlib.Path) -> None:
  repo.mkdir(parents=True)
  _git(repo, "init", "-q", "-b", "main")
  (repo / "stray.txt").write_text("outside the workspace\n", encoding="utf-8")
  _commit(repo, "stray", BASE_TIME)
  _reset_index(repo)


async def build(b: Builder) -> dict:
  tree = b.cfg.charliebot_home.parent
  workspace = tree / "workspace"
  _build_repo(workspace / "diff-repo")
  plain = workspace / "plain-dir"
  plain.mkdir()
  (plain / "notes.txt").write_text("Not a repository.\n", encoding="utf-8")
  _build_stray(tree / "stray-repo")
  code_server = tree / "bin" / "code-server"
  code_server.parent.mkdir()
  code_server.write_text("#!/bin/sh\nexit 3\n", encoding="utf-8")
  code_server.chmod(0o755)
  (tree / "code-server.yaml").write_text(CODE_SERVER_CONFIG, encoding="utf-8")
  return {
      "repo": "workspace/diff-repo",
      "git_dir": "workspace/diff-repo/.git",
      "plain_dir": "workspace/plain-dir",
      "missing_dir": "workspace/no-such-dir",
      "stray_repo": "stray-repo",
  }
