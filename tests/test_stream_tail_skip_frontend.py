"""Pytest wrapper for the stream-tail highlight-skip node suite."""

from pathlib import Path

from conftest import run_node_js_test


def test_stream_tail_skip_frontend() -> None:
  run_node_js_test(
      Path(__file__).parent / "stream_tail_skip.test.js",
      skip_reason="node is not installed on this host",
  )
