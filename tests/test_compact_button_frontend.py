from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'compact_button.test.js'


def test_compact_button_frontend() -> None:
  """Run focused frontend compact-button tests against the chat/sidebar JS."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for compact button frontend tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
