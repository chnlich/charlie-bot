from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'session_view_sentinel.test.js'


def test_session_view_sentinel_node() -> None:
  """Run frontend sentinel state acceptance tests against session-view.js."""
  run_node_js_test(NODE_TEST, 'node is required for session view sentinel tests')
