from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'worker_events_incremental.test.js'


def test_worker_events_incremental_frontend() -> None:
  """Run frontend tests for the incremental worker-events fetch and append paint."""
  run_node_js_test(NODE_TEST, 'node is required for worker-events incremental frontend tests')
