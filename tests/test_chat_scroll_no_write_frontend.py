from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_scroll_no_write.test.js'


def test_chat_scroll_no_write_frontend() -> None:
  """Run the zero-DOM-write guard tests against scroll.js/status.js/workers.js."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for chat scroll no-write frontend tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
