from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'workers_list_conditional_poll.test.js'


def test_workers_list_conditional_poll_frontend() -> None:
  run_node_js_test(NODE_TEST, 'node is required for workers list conditional poll frontend tests')
