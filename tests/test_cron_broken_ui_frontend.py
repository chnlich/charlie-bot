from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'cron_broken_ui.test.js'


def test_cron_broken_ui_frontend() -> None:
  """Run the broken-cron badge/modal tests against sidebar groups.js + modals.js."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for cron broken UI frontend tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
