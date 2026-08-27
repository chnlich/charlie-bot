from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'websocket_catchup_split_invariance.test.js'


def test_websocket_catchup_split_invariance_node() -> None:
  """Run focused frontend catchup-split-invariance tests against websocket.js."""
  run_node_js_test(NODE_TEST, 'node is required for websocket catchup split-invariance tests')
