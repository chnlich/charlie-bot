"""CLI help stays fast: registering the packages imports neither pydantic nor ``dataclasses``.

``charliebot --help`` and the ``--help`` of a subcommand register the packages before they print. A package
registers strings only, so no help command loads the pydantic models, and the modules on the registration path
define their value types without ``dataclasses``. Each help command runs in a fresh interpreter, because one
import of either module in this process would hide a regression.
"""

import os
import subprocess
import sys
from pathlib import Path

import conftest
import pytest

_SCRIPT = """
import sys
from src.app.main import main

try:
  main(sys.argv[1:])
except SystemExit:
  pass  # argparse exits after printing a subcommand's help
print(sorted(name for name in ("pydantic", "dataclasses") if name in sys.modules))
"""


@pytest.mark.parametrize("argv", [["--help"], ["usage-ledger", "--help"], ["storage", "--help"]])
def test_help_imports_no_pydantic_and_no_dataclasses(argv: list[str], tmp_path: Path) -> None:
  env = {**os.environ, "CHARLIEBOT_HOME": str(tmp_path)}

  result = subprocess.run(
      [sys.executable, "-c", _SCRIPT, *argv],
      cwd=conftest.ROOT,
      env=env,
      capture_output=True,
      text=True,
      check=True,
      timeout=60)

  assert result.stdout.splitlines()[-1] == "[]"
