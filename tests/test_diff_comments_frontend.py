from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'diff_comments.test.js'


def test_diff_comments_node() -> None:
  """Run focused diff_comments.js vm tests."""
  run_node_js_test(NODE_TEST, 'node is required for diff comments tests')
