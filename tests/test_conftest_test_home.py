"""The suite's throwaway HOME lasts only as long as its process; block accessors build only under a test's tmp dir."""
import os
import pathlib
import subprocess
import sys

import pytest

from src.backends.claude_code import claude_code
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


def test_a_pytest_run_started_with_the_gateway_variables_loses_them(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A pytest process launched inside a cc-anthropic-compatible worker inherits the worker's
  gateway variables; its conftest strips them, so the tests it runs see none of them."""
  inner = tmp_path / "test_gateway_variables_are_stripped.py"
  inner.write_text(
      "\n".join(
          [
              "import os",
              "from src.backends.claude_code import claude_code",
              "",
              "",
              "def test_gateway_variables_are_stripped():",
              "  for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES:",
              "    assert name not in os.environ, name",
          ]),
      encoding="utf-8")
  env = {
      **os.environ,
      **dict.fromkeys(claude_code.GATEWAY_ENDPOINT_ENV_NAMES, "inherited-gateway-value"),
      # tests/conftest.py rides the PYTHONPATH so `-p conftest` loads it into the inner run.
      "PYTHONPATH":
          os.pathsep.join([str(ROOT / "tests"), os.environ.get("PYTHONPATH", "")]).rstrip(os.pathsep),
  }

  out = subprocess.run(
      [sys.executable, "-m", "pytest",
       str(inner), "-q", "-p", "conftest", "-p", "no:cacheprovider"],
      cwd=ROOT,
      env=env,
      capture_output=True,
      text=True,
      timeout=60,
  )

  assert out.returncode == 0, out.stdout + out.stderr


def test_a_block_accessor_fails_the_test_when_its_config_home_is_outside_the_tmp_dir(block_home_violations):
  # The per-test profile home that get_config() reads sits beside tmp_path, not under it.
  with pytest.raises(AssertionError, match="outside this test's tmp dir"):
    session_store.store()

  assert len(block_home_violations) == 1  # the teardown check fails any test that leaves one recorded
  block_home_violations.clear()
