from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'artifacts_link_behavior.test.js'


def test_artifacts_link_behavior_frontend() -> None:
  """Run focused frontend tests for HTML artifact link behavior injection."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for artifact link behavior frontend tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
