"""The pytest plugin that the local work guard loads through PYTEST_PLUGINS: refuses a pytest run on this host.

`--version` and `--help` run as usual. A refused run prints the guard's refusal and exits with 4, the code pytest uses
for a usage error.
"""

import subprocess
from pathlib import Path

import pytest

REFUSE = Path(__file__).resolve().parent.parent / "refuse.sh"


@pytest.hookimpl(tryfirst=True)
def pytest_cmdline_main(config):
  if config.option.version or config.option.help:
    return None
  subprocess.run(["bash", str(REFUSE), "pytest"], check=False)
  return 4
