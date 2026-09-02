"""Pytest wrapper for the stream-draft coalescing node suite."""

from pathlib import Path

from conftest import run_node_js_test


def test_usage_stream_render_frontend() -> None:
  run_node_js_test(
      Path(__file__).parent / "usage_stream_render.test.js",
      skip_reason="node is not installed on this host",
  )
