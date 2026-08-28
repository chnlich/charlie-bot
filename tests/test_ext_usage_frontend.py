from __future__ import annotations

from pathlib import Path

from conftest import run_node_js_test

ROOT = Path(__file__).resolve().parents[1]
NODE_TEST = ROOT / 'tests' / 'ext_usage_render.test.mjs'


def test_ext_usage_frontend_rendering() -> None:
  run_node_js_test(NODE_TEST, 'node is required for ext usage frontend tests')
