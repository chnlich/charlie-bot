"""Pytest wrapper for the renderer highlight-cache node suite."""

from pathlib import Path

from conftest import run_node_js_test


def test_marked_hl_cache_frontend() -> None:
  run_node_js_test(
      Path(__file__).parent / "marked_hl_cache.test.js",
      skip_reason="node is not installed on this host",
  )
