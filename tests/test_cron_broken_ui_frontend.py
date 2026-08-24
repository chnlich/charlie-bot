from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'cron_broken_ui.test.js'


def test_cron_broken_ui_frontend() -> None:
  """Run the broken-cron badge/modal tests against sidebar groups.js + modals.js."""
  run_node_js_test(NODE_TEST, 'node is required for cron broken UI frontend tests')
