from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_session_bump.test.js'


def test_chat_session_bump_frontend() -> None:
  """Run focused frontend sidebar bump tests against chat.js."""
  run_node_js_test(NODE_TEST, 'node is required for chat session bump frontend tests')
