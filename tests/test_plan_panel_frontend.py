from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'plan_panel.test.js'


def test_plan_panel_frontend() -> None:
  """Run focused frontend tests for the plan panel module."""
  run_node_js_test(NODE_TEST, 'node is required for plan panel frontend tests')
