from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_delegate_rendering.test.js'


def test_chat_delegate_rendering_frontend() -> None:
  run_node_js_test(NODE_TEST, 'node is required for chat delegate rendering frontend tests')
