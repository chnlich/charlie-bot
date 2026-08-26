from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'show_more_toggle.test.js'


def test_show_more_toggle_frontend() -> None:
  """Run focused frontend tests for the shared show-more toggle helper."""
  run_node_js_test(NODE_TEST, 'node is required for show-more toggle frontend tests')
