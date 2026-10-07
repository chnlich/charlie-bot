"""The suite's throwaway HOME (tests/conftest.py) lasts only as long as the process that created it."""
import os
import pathlib
import subprocess
import sys

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[1]


@pytest.mark.integration
def test_test_home_is_deleted_when_the_importing_process_exits():
  # A fresh process imports conftest the way pytest does, reports its HOME, and exits normally.
  probe = "import os, sys; sys.path.insert(0, 'tests'); import conftest; h = os.environ['HOME']; print(h, os.path.isdir(h))"
  out = subprocess.run([sys.executable, "-c", probe], cwd=ROOT, capture_output=True, text=True, check=True, timeout=60)
  home, existed = out.stdout.split()[-2:]
  assert pathlib.Path(home).name.startswith("charliebot-test-home-")
  assert home != os.environ["HOME"]  # the probe made its own home rather than reusing this run's
  assert existed == "True"
  assert not pathlib.Path(home).exists()
