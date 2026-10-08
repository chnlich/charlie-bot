"""The suite's throwaway HOME lasts only as long as its process; block accessors build only under a test's tmp dir."""
import os
import pathlib
import subprocess
import sys

import pytest

from src.runtime import session_store

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


def test_a_block_accessor_fails_the_test_when_its_config_home_is_outside_the_tmp_dir(block_home_violations):
  # The per-test profile home that get_config() reads sits beside tmp_path, not under it.
  with pytest.raises(AssertionError, match="outside this test's tmp dir"):
    session_store.store()

  assert len(block_home_violations) == 1  # the teardown check fails any test that leaves one recorded
  block_home_violations.clear()
