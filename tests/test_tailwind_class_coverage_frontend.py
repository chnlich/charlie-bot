from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'tailwind_class_coverage.test.js'


def test_tailwind_class_coverage_frontend() -> None:
  """Run the Tailwind class-coverage test against the committed tailwind.css."""
  run_node_js_test(NODE_TEST, 'node is required for tailwind class coverage frontend tests')
