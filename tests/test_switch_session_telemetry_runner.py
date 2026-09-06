from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'test_switch_session_telemetry.test.js'


def test_switch_session_telemetry_node() -> None:
  """Run frontend switch-telemetry acceptance tests against session-view.js."""
  run_node_js_test(NODE_TEST, 'node is required for switch session telemetry tests')
