from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'artifact_comment_drafts.test.js'


def test_artifact_comment_drafts_frontend() -> None:
  """Run focused frontend tests for the artifact comment tray draft persistence."""
  run_node_js_test(NODE_TEST, 'node is required for artifact comment draft frontend tests')
