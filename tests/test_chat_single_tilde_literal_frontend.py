from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_single_tilde_literal.test.js'


def test_chat_single_tilde_literal_node() -> None:
  """Run focused single-tilde literal rendering tests."""
  run_node_js_test(NODE_TEST, 'node is required for chat single-tilde literal tests')
