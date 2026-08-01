from __future__ import annotations

import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
BUILD_SCRIPT = ROOT / 'scripts' / 'build-css.sh'
COMMITTED_CSS = ROOT / 'web' / 'static' / 'css' / 'tailwind.css'


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
