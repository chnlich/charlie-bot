import shutil
import subprocess
from pathlib import Path

import pytest


def test_ext_usage_frontend_rendering() -> None:
  node = shutil.which("node")
  if node is None:
    pytest.skip("node is required for ext usage frontend tests")
  repo_root = Path(__file__).resolve().parent.parent
  subprocess.run(
      [node, "--test", "tests/ext_usage_render.test.mjs"],
      check=True,
      cwd=repo_root,
  )
