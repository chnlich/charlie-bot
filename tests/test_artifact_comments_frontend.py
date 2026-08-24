from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'artifact_comments.test.js'


def test_artifact_comments_frontend() -> None:
  """Run frontend tests for the artifact comments guard and tray logic."""
  run_node_js_test(NODE_TEST, 'node is required for artifact comments frontend tests')
