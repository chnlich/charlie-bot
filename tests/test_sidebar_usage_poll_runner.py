from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'sidebar_usage_poll.test.js'


def test_sidebar_usage_poll_node() -> None:
  """Run focused frontend polling tests against sidebar.js."""
  run_node_js_test(NODE_TEST, 'node is required for sidebar usage polling tests')
