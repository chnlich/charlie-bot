"""Pytest wrapper for the archived-view node suite."""

from pathlib import Path

from conftest import run_node_js_test


def test_archived_view_frontend() -> None:
  run_node_js_test(
      Path(__file__).parent / "test_archived_view.test.js",
      skip_reason="node is not installed on this host",
  )
