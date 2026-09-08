"""Pytest entry for the node --test frontend suites: one case per listed JS test file."""

from pathlib import Path

import pytest
from conftest import run_node_js_test

# Keep this set disjoint from the suites still wrapped by tests/test_*_frontend.py, or a suite runs twice.
_NODE_TESTS = [
    "artifact_comment_drafts.test.js",
    "artifact_comments.test.js",
    "artifacts_link_behavior.test.js",
    "backend_badge_switch.test.js",
    "chat_artifact_cards.test.js",
    "chat_attachments_render.test.js",
    "chat_delegate_rendering.test.js",
    "chat_file_link_prefixes.test.js",
    "chat_markup_containment.test.js",
    "chat_scroll_no_write.test.js",
    "chat_session_bump.test.js",
    "chat_single_tilde_literal.test.js",
    "chat_url_ascii_boundary.test.js",
    "comment_post.test.js",
    "compact_button.test.js",
    "cron_broken_ui.test.js",
    "diff_comments.test.js",
    "ext_usage_render.test.mjs",
    "marked_hl_cache.test.js",
    "page_timers_visibility.test.js",
]


@pytest.mark.parametrize("js_name", _NODE_TESTS)
def test_frontend_js(js_name: str) -> None:
  run_node_js_test(Path(__file__).parent / js_name, "node is required for the frontend JS tests")
