from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_artifact_cards.test.js'


def test_chat_artifact_cards_node() -> None:
  """Run the compact-artifact-card tests against chat/artifacts.js."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for the chat artifact card tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
