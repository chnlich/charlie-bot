from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_file_link_prefixes.test.js'


def test_chat_file_link_prefixes_node() -> None:
  """Run the dual-prefix parse, marking and probe-budget tests against chat/artifacts.js."""
  run_node_js_test(NODE_TEST, 'node is required for the chat file link prefix tests')
