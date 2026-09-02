from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_url_ascii_boundary.test.js'


def test_chat_url_ascii_boundary_node() -> None:
  """Run the bare-URL ASCII-boundary rendering and cross-layer tests."""
  run_node_js_test(NODE_TEST, 'node is required for chat URL ASCII-boundary tests')
