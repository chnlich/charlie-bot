from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'terminal_b64.test.js'


def test_terminal_b64_frontend() -> None:
  """Run focused frontend tests for the shared terminal base64 helpers."""
  run_node_js_test(NODE_TEST, 'node is required for terminal base64 helper tests')
