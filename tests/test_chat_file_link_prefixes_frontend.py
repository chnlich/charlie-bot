from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_file_link_prefixes.test.js'


def test_chat_file_link_prefixes_node() -> None:
  """Run the dual-prefix parse, marking and probe-budget tests against chat/artifacts.js."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for the chat file link prefix tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
