from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'panel_resize.test.js'


def test_panel_resize_frontend() -> None:
  """Run focused frontend tests for the shared panel-resize helper."""
  run_node_js_test(NODE_TEST, 'node is required for panel resize frontend tests')
