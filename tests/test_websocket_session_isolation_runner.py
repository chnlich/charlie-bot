from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'websocket_session_isolation.test.js'


def test_websocket_session_isolation_node() -> None:
  """Run focused frontend session-isolation tests against websocket.js."""
  run_node_js_test(NODE_TEST, 'node is required for websocket session-isolation tests')
