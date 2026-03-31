import subprocess
from pathlib import Path


def test_ext_usage_frontend_rendering() -> None:
  repo_root = Path(__file__).resolve().parent.parent
  subprocess.run(
      ["node", "--test", "tests/ext_usage_render.test.mjs"],
      check=True,
      cwd=repo_root,
  )
