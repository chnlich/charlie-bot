from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'plan_cards.test.js'


def test_plan_cards_frontend() -> None:
  """Run focused frontend tests for chat plan compact-card helpers."""
  run_node_js_test(NODE_TEST, 'node is required for plan card frontend tests')
