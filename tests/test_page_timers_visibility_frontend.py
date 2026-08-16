from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'page_timers_visibility.test.js'


def test_page_timers_visibility_node() -> None:
  """Run the hidden-tab timer tests against page-timers.js, sidebar.js and app.js."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for the page timer visibility tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
