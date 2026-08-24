from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_scroll_no_write.test.js'


def test_chat_scroll_no_write_frontend() -> None:
  """Run the zero-DOM-write guard tests against scroll.js/status.js/workers.js."""
  run_node_js_test(NODE_TEST, 'node is required for chat scroll no-write frontend tests')
