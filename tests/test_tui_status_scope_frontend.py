from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'tui_status_scope.test.js'


def test_tui_status_scope_frontend() -> None:
  """Run focused frontend tests for tui/status poll scoping against sidebar JS."""
  run_node_js_test(NODE_TEST, 'node is required for tui status scope frontend tests')
