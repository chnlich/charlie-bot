from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'artifacts_link_behavior.test.js'


def test_artifacts_link_behavior_frontend() -> None:
  """Run focused frontend tests for HTML artifact link behavior injection."""
  run_node_js_test(NODE_TEST, 'node is required for artifact link behavior frontend tests')
