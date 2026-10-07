"""Tests for extract_review_context — codex empty-result fallback and partial-context contract."""

import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.runtime import review


def _setup_paths(tmp_path: pathlib.Path, session_id: str, thread_id: str) -> tuple[pathlib.Path, pathlib.Path]:
  chat_log = tmp_path / session_id / "data" / "chat_events.jsonl"
  worker_log = tmp_path / session_id / "threads" / thread_id / "data" / "events.jsonl"
  return chat_log, worker_log


@pytest.mark.asyncio
async def test_codex_style_falls_back_to_assistant_text(tmp_path: pathlib.Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  conftest.append_events(chat_log, [{"type": ET.TASK_DELEGATED, "thread_id": thread_id, "description": "Do X"}])
  conftest.append_events(
      worker_log, [conftest.assistant_text_event("Done. Commit abcdef."), {
          "type": ET.RESULT,
          "result": ""
      }])

  user_request, worker_summary = await review.extract_review_context(session_id, thread_id, tmp_path)
  assert user_request == "Do X"
  assert worker_summary == "Done. Commit abcdef."


@pytest.mark.asyncio
async def test_worker_summary_preserved_when_no_task_delegated(tmp_path: pathlib.Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  conftest.append_events(chat_log, [])
  conftest.append_events(worker_log, [{"type": ET.RESULT, "result": "All done."}])

  user_request, worker_summary = await review.extract_review_context(session_id, thread_id, tmp_path)
  assert user_request is None
  assert worker_summary == "All done."


@pytest.mark.asyncio
async def test_worker_summary_prefers_newest_assistant_over_older_result(tmp_path: pathlib.Path) -> None:
  # The scan is newest-first and stops at the first event carrying text: an
  # assistant message newer than a result wins, whichever kind came first.
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  conftest.append_events(chat_log, [{"type": ET.TASK_DELEGATED, "thread_id": thread_id, "description": "Do X"}])
  conftest.append_events(
      worker_log, [
          {
              "type": ET.RESULT,
              "result": "older result text"
          },
          conftest.assistant_text_event("newest words"),
      ])

  _, worker_summary = await review.extract_review_context(session_id, thread_id, tmp_path)
  assert worker_summary == "newest words"


@pytest.mark.asyncio
async def test_worker_summary_skips_malformed_tail_lines(tmp_path: pathlib.Path) -> None:
  # The from-the-end walk skips blank and malformed lines; a torn tail must
  # not hide the result event below it.
  session_id, thread_id = "sess-1", "thr-1"
  chat_log, worker_log = _setup_paths(tmp_path, session_id, thread_id)
  conftest.append_events(chat_log, [{"type": ET.TASK_DELEGATED, "thread_id": thread_id, "description": "Do X"}])
  conftest.append_events(worker_log, [{"type": ET.RESULT, "result": "Worker did X successfully."}])
  with open(worker_log, "a", encoding="utf-8") as stream:
    stream.write("\n{broken json\n")

  _, worker_summary = await review.extract_review_context(session_id, thread_id, tmp_path)
  assert worker_summary == "Worker did X successfully."
