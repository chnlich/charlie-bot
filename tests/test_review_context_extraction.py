"""Tests for extract_review_context — codex empty-result fallback and partial-context contract."""

from pathlib import Path

import pytest
from conftest import append_events

from src.core import event_types as ET
from src.core.review import extract_review_context


def _setup_paths(tmp_path: Path, session_id: str, thread_id: str) -> tuple[Path, Path]:
  chat_log = tmp_path / session_id / "data" / "chat_events.jsonl"
  worker_log = tmp_path / session_id / "threads" / thread_id / "data" / "events.jsonl"
  return chat_log, worker_log


@pytest.mark.asyncio
async def test_claude_style_full_context(tmp_path: Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  append_events(chat_log, [{"type": ET.TASK_DELEGATED, "thread_id": thread_id, "description": "Do X"}])
  append_events(worker_log, [{"type": ET.RESULT, "result": "Worker did X successfully."}])

  user_request, worker_summary = await extract_review_context(session_id, thread_id, tmp_path)
  assert user_request == "Do X"
  assert worker_summary == "Worker did X successfully."


@pytest.mark.asyncio
async def test_codex_style_falls_back_to_assistant_text(tmp_path: Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  append_events(chat_log, [{"type": ET.TASK_DELEGATED, "thread_id": thread_id, "description": "Do X"}])
  append_events(worker_log, [
      {
          "type": ET.ASSISTANT,
          "message": {"content": [{"type": "text", "text": "Done. Commit abcdef."}]},
      },
      {"type": ET.RESULT, "result": ""},
  ])

  user_request, worker_summary = await extract_review_context(session_id, thread_id, tmp_path)
  assert user_request == "Do X"
  assert worker_summary == "Done. Commit abcdef."


@pytest.mark.asyncio
async def test_user_request_preserved_when_no_worker_signal(tmp_path: Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  append_events(chat_log, [{"type": ET.TASK_DELEGATED, "thread_id": thread_id, "description": "Do X"}])
  append_events(worker_log, [
      {"type": ET.TOOL_USE, "name": "Bash"},
      {"type": ET.TOOL_RESULT, "content": "ok"},
  ])

  user_request, worker_summary = await extract_review_context(session_id, thread_id, tmp_path)
  assert user_request == "Do X"
  assert worker_summary is None


@pytest.mark.asyncio
async def test_worker_summary_preserved_when_no_task_delegated(tmp_path: Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  append_events(chat_log, [])
  append_events(worker_log, [{"type": ET.RESULT, "result": "All done."}])

  user_request, worker_summary = await extract_review_context(session_id, thread_id, tmp_path)
  assert user_request is None
  assert worker_summary == "All done."


@pytest.mark.asyncio
async def test_both_missing_returns_none_none(tmp_path: Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  append_events(chat_log, [])
  append_events(worker_log, [])

  user_request, worker_summary = await extract_review_context(session_id, thread_id, tmp_path)
  assert user_request is None
  assert worker_summary is None
