from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'rendering_worker_summary_origin.test.js'


def test_rendering_worker_summary_origin_node() -> None:
  """Run frontend worker-summary origin footer acceptance tests against chat/rendering.js."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for rendering worker-summary origin tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')