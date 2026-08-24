from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'page_timers_visibility.test.js'


def test_page_timers_visibility_node() -> None:
  """Run the hidden-tab timer tests against page-timers.js, sidebar.js and app.js."""
  run_node_js_test(NODE_TEST, 'node is required for the page timer visibility tests')
