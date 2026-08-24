from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'chat_artifact_cards.test.js'


def test_chat_artifact_cards_node() -> None:
  """Run the compact-artifact-card tests against chat/artifacts.js."""
  run_node_js_test(NODE_TEST, 'node is required for the chat artifact card tests')
