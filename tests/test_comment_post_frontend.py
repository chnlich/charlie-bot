from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'comment_post.test.js'


def test_comment_post_node() -> None:
  """Run focused comment_post.js vm tests."""
  run_node_js_test(NODE_TEST, 'node is required for comment post tests')
