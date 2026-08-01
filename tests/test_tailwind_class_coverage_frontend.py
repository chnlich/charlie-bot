from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'tailwind_class_coverage.test.js'


def test_tailwind_class_coverage_frontend() -> None:
  """Run the Tailwind class-coverage test against the committed tailwind.css."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for tailwind class coverage frontend tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
