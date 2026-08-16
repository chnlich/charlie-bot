from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'artifact_comment_drafts.test.js'


def test_artifact_comment_drafts_frontend() -> None:
  """Run focused frontend tests for the artifact comment tray draft persistence."""
  node = shutil.which('node')
  if node is None:
    pytest.skip('node is required for artifact comment draft frontend tests')

  result = subprocess.run(
      [node, '--test', str(NODE_TEST)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')
