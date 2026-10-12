"""Tests for extract_review_context — codex empty-result fallback and the worker-summary contract."""

import pathlib

import conftest
import pytest

from src.infra import event_types as ET
from src.runtime import review


def _worker_log_path(tmp_path: pathlib.Path, session_id: str, thread_id: str) -> pathlib.Path:
  return tmp_path / session_id / "data" / "runs" / thread_id / "events.jsonl"


@pytest.mark.asyncio
async def test_codex_style_falls_back_to_assistant_text(tmp_path: pathlib.Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  worker_log = _worker_log_path(tmp_path, session_id, thread_id)
  conftest.append_events(
      worker_log, [conftest.assistant_text_event("Done. Commit abcdef."), {
          "type": ET.RESULT,
          "result": ""
      }])

  worker_summary = await review.extract_review_context(session_id, thread_id, worker_log)
  assert worker_summary == "Done. Commit abcdef."


@pytest.mark.asyncio
async def test_result_text_is_the_worker_summary(tmp_path: pathlib.Path) -> None:
  session_id, thread_id = "sess-1", "thr-1"
  worker_log = _worker_log_path(tmp_path, session_id, thread_id)
  conftest.append_events(worker_log, [{"type": ET.RESULT, "result": "All done."}])

  assert await review.extract_review_context(session_id, thread_id, worker_log) == "All done."


@pytest.mark.asyncio
async def test_worker_summary_prefers_newest_assistant_over_older_result(tmp_path: pathlib.Path) -> None:
  # The scan is newest-first and stops at the first event carrying text: an
  # assistant message newer than a result wins, whichever kind came first.
  session_id, thread_id = "sess-1", "thr-1"
  worker_log = _worker_log_path(tmp_path, session_id, thread_id)
  conftest.append_events(
      worker_log, [
          {
              "type": ET.RESULT,
              "result": "older result text"
          },
          conftest.assistant_text_event("newest words"),
      ])

  assert await review.extract_review_context(session_id, thread_id, worker_log) == "newest words"


@pytest.mark.asyncio
async def test_worker_summary_skips_malformed_tail_lines(tmp_path: pathlib.Path) -> None:
  # The from-the-end walk skips blank and malformed lines; a torn tail must
  # not hide the result event below it.
  session_id, thread_id = "sess-1", "thr-1"
  worker_log = _worker_log_path(tmp_path, session_id, thread_id)
  conftest.append_events(worker_log, [{"type": ET.RESULT, "result": "Worker did X successfully."}])
  with open(worker_log, "a", encoding="utf-8") as stream:
    stream.write("\n{broken json\n")

  assert await review.extract_review_context(session_id, thread_id, worker_log) == "Worker did X successfully."
