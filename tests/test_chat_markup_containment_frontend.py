from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_markup_containment.test.js'


def test_chat_markup_containment_node() -> None:
  """Run focused markdown-renderer markup-containment tests."""
  run_node_js_test(NODE_TEST, 'node is required for chat markup containment tests')
