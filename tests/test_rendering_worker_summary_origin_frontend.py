from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'rendering_worker_summary_origin.test.js'


def test_rendering_worker_summary_origin_node() -> None:
  """Run frontend worker-summary origin footer acceptance tests against chat/rendering.js."""
  run_node_js_test(NODE_TEST, 'node is required for rendering worker-summary origin tests')
