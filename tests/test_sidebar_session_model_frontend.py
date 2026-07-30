from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'sidebar_session_model.test.js'


def test_sidebar_session_model_frontend() -> None:
  """Run the session-row model label tests against sidebar/groups.js."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for sidebar session model frontend tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
