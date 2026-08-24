from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'compact_button.test.js'


def test_compact_button_frontend() -> None:
  """Run focused frontend compact-button tests against the chat/sidebar JS."""
  run_node_js_test(NODE_TEST, 'node is required for compact button frontend tests')
