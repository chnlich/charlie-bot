from __future__ import annotations

import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
BUILD_SCRIPT = ROOT / 'scripts' / 'build-css.sh'
COMMITTED_CSS = ROOT / 'web' / 'static' / 'css' / 'tailwind.css'
COMMITTED_LOCK = ROOT / 'package-lock.json'


def _need_node() -> bool:
  return shutil.which('node') is not None


def test_tailwind_css_build_matches_committed_output() -> None:
  """Rebuild web/static/css/tailwind.css from the repo's own config/globs into a
  temp path and assert byte-identical output — catches a committed CSS that has
  gone stale relative to the class usage in templates/JS, or was hand-edited.
  """
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for the tailwind css build test')

  with tempfile.TemporaryDirectory() as tmpdir:
    out_path = Path(tmpdir) / 'tailwind.css'
    result = subprocess.run(
        [str(BUILD_SCRIPT), str(out_path)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
      pytest.fail(f'build-css.sh failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')

    rebuilt = out_path.read_bytes()

  committed = COMMITTED_CSS.read_bytes()
  assert rebuilt == committed, (
      'committed web/static/css/tailwind.css is stale — rerun scripts/build-css.sh and commit the result'
  )


def test_package_lock_not_rewritten_by_npm_build() -> None:
  """The npm install inside build-css.sh must leave package-lock.json byte-identical.

  package.json declares an explicit name ("charlie-bot"); without one npm derives
  the lockfile's name from the containing directory (the checkout/worktree name)
  and rewrites package-lock.json on every run. Run build-css.sh via a subprocess
  and compare the lockfile to its committed bytes.
  """
  if not _need_node():
    pytest.skip('node is required for the npm build test')

  committed = subprocess.run(
      ['git', 'show', 'HEAD:package-lock.json'],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=True,
  ).stdout.encode()

  with tempfile.TemporaryDirectory() as tmpdir:
    result = subprocess.run(
        [str(BUILD_SCRIPT), str(Path(tmpdir) / 'tailwind.css')],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
      pytest.fail(f'build-css.sh failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')

  assert COMMITTED_LOCK.read_bytes() == committed, (
      'npm run inside build-css.sh rewrote package-lock.json — '
      'ensure package.json declares explicit "name": "charlie-bot"'
  )
