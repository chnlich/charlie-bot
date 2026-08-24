from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'backend_badge_switch.test.js'


def test_backend_badge_switch_frontend() -> None:
  """Run focused frontend backend-badge-switch tests against the sidebar JS."""
  run_node_js_test(NODE_TEST, 'node is required for backend badge switch frontend tests')
