"""Pytest entry for the node --test frontend suites: one case per listed JS test file."""

import shutil
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]


def run_node_js_test(node_test: Path, skip_reason: str) -> None:
  """Run one node --test file; hosts without node skip rather than fail, and cwd=ROOT keeps repo-relative asset
  loads working."""
  node = shutil.which('node')
  if node is None:
    pytest.skip(skip_reason)

  result = subprocess.run(
      [node, '--test', str(node_test)],
      cwd=ROOT,
      capture_output=True,
      text=True,
      check=False,
      # The suites finish in ~1s; the bound turns a hung node child into a test failure instead of a CI hang.
      timeout=300,
  )
  if result.returncode != 0:
    pytest.fail(f'Node tests failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}')


# One entry per node suite under tests/: an omitted suite silently stops running, a duplicate entry runs twice.
_NODE_TESTS = [
    "artifact_comment_drafts.test.js",
    "artifact_comments.test.js",
    "artifacts_link_behavior.test.js",
    "backend_badge_switch.test.js",
    "chat_artifact_cards.test.js",
    "chat_attachments_render.test.js",
    "chat_delegate_rendering.test.js",
    "chat_file_link_prefixes.test.js",
    "chat_link_prefix_gate.test.js",
    "chat_math_extension.test.js",
    "chat_math_gate.test.js",
    "chat_markup_containment.test.js",
    "chat_scroll_no_write.test.js",
    "chat_session_bump.test.js",
    "chat_single_tilde_literal.test.js",
    "chat_switch_tool_preview.test.js",
    "chat_url_ascii_boundary.test.js",
    "code_block_wc2ch.test.js",
    "comment_post.test.js",
    "compact_button.test.js",
    "cron_broken_ui.test.js",
    "diff_comments.test.js",
    "ext_usage_render.test.mjs",
    "leaf_view.test.js",
    "marked_hl_cache.test.js",
    "page_timers_visibility.test.js",
    "panel_resize.test.js",
    "plan_cards.test.js",
    "plan_panel.test.js",
    "prose_markdown_memo.test.js",
    "prose_math_memo.test.js",
    "rendering_worker_summary_origin.test.js",
    "session_switch_stale_pagination.test.js",
    "session_view_create_task.test.js",
    "session_view_leaf.test.js",
    "session_view_sentinel.test.js",
    "show_more_toggle.test.js",
    "sidebar_indicator_priority.test.js",
    "sidebar_mark_read.test.js",
    "sidebar_rename_prefill.test.js",
    "sidebar_session_model.test.js",
    "sidebar_tree_nesting.test.js",
    "sidebar_worker_thread_leaf.test.js",
    "sidebar_usage_poll.test.js",
    "stream_incremental_parse.test.js",
    "stream_tail_skip.test.js",
    "tailwind_class_coverage.test.js",
    "task_context_dialog.test.js",
    "terminal_b64.test.js",
    "terminal_mount.test.js",
    "test_archived_view.test.js",
    "test_sidebar_delete_backfill.test.js",
    "test_switch_session_telemetry.test.js",
    "thinking_toggle.test.js",
    "trigger_fire_time_roundtrip.test.js",
    "tui_status_scope.test.js",
    "usage_stream_render.test.js",
    "voice_backend_menu.test.js",
    "voice_input_run.test.js",
    "worker_description_prefix.test.js",
    "worker_events_incremental.test.js",
    "worker_events_metadata_failure.test.js",
    "worker_events_truncation_note.test.js",
    "worker_toggle_collapse.test.js",
    "websocket_catchup_split_invariance.test.js",
    "websocket_session_isolation.test.js",
]


@pytest.mark.parametrize("js_name", _NODE_TESTS)
def test_frontend_js(js_name: str) -> None:
  run_node_js_test(Path(__file__).parent / js_name, "node is required for the frontend JS tests")


def test_node_tests_list_covers_every_suite() -> None:
  """``_NODE_TESTS`` matches the node suites on disk exactly: no omission, no duplicate, no stale entry."""
  on_disk = sorted(p.name for pattern in ("*.test.js", "*.test.mjs") for p in Path(__file__).parent.glob(pattern))
  assert sorted(_NODE_TESTS) == on_disk
