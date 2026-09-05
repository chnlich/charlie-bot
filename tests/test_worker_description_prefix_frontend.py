from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'worker_description_prefix.test.js'


def test_worker_description_prefix_frontend() -> None:
  run_node_js_test(NODE_TEST, 'node is required for worker description prefix frontend tests')
