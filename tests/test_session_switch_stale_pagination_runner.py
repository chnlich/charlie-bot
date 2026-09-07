from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'session_switch_stale_pagination.test.js'


def test_session_switch_stale_pagination_node() -> None:
  """Run the stale cross-session pagination race regression tests against session-view.js."""
  run_node_js_test(NODE_TEST, 'node is required for session switch stale pagination tests')
