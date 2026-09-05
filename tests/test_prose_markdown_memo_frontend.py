"""Pytest wrapper for the message-body markdown parse memo node suite."""

from pathlib import Path

from conftest import run_node_js_test


def test_prose_markdown_memo_frontend() -> None:
  run_node_js_test(
      Path(__file__).parent / "prose_markdown_memo.test.js",
      skip_reason="node is not installed on this host",
  )
