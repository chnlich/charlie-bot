from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_attachments_render.test.js'


def test_chat_attachments_frontend() -> None:
  run_node_js_test(NODE_TEST, 'node is required for chat attachment frontend tests')
