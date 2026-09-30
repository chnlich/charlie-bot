"""Pytest entry for the node --test frontend suites: one case per JS suite file on disk."""

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]


def run_node_js_test(node_test: Path, skip_reason: str) -> None:
  """Run one node --test file; hosts without node skip rather than fail, and cwd=ROOT keeps repo-relative asset
  loads working."""
  node = shutil.which('node')
  if node is None:
    pytest.skip(skip_reason)

  result = subprocess.run(
      [node, '--test', str(node_test)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
      # The suites finish in ~1s; the bound turns a hung node child into a test failure instead of a CI hang.
      timeout=300,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')


# Node suites that exceed the 1s unit budget on this measurement (a node
# subprocess plus its own suite runtime): each carries the integration marker
# via pytest.param below instead of dragging every suite's case over the cap.
_INTEGRATION_SUITES = {
    "chat_session_bump.test.js",
    "tailwind_class_coverage.test.js",
    "stream_incremental_parse.test.js",
    # The archived view's cap walk: 21 keyset page loads whose merged tree
    # re-renders up to ~2100 rows per load — the walk the 2000-row render cap
    # bounds, far past a unit test's shape.
    "test_archived_view.test.js",
}

# One case per node suite on disk: the glob is the single source, so a suite file that
# lands runs in the bridge without a registration edit; sorted() pins the case order
# across hosts.
_NODE_TESTS = sorted(p.name for pattern in ("*.test.js", "*.test.mjs") for p in Path(__file__).parent.glob(pattern))


@pytest.mark.parametrize(
    "js_name", [
        pytest.param(name, marks=pytest.mark.integration) if name in _INTEGRATION_SUITES else name
        for name in _NODE_TESTS
    ])
def test_frontend_js(js_name: str) -> None:
  run_node_js_test(Path(__file__).parent / js_name, "node is required for the frontend JS tests")
