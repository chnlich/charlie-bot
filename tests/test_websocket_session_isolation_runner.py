from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'websocket_session_isolation.test.js'


def test_websocket_session_isolation_node() -> None:
  """Run focused frontend session-isolation tests against websocket.js."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for websocket session-isolation tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
