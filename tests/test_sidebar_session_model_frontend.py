from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'sidebar_session_model.test.js'


def test_sidebar_session_model_frontend() -> None:
  """Run the session-row model label tests against sidebar/groups.js."""
  run_node_js_test(NODE_TEST, 'node is required for sidebar session model frontend tests')
