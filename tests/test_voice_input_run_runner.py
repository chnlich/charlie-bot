from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'voice_input_run.test.js'


def test_voice_input_run_node() -> None:
  """Run focused frontend run-structure tests against voice-input.js."""
  run_node_js_test(NODE_TEST, 'node is required for voice input run-structure tests')
