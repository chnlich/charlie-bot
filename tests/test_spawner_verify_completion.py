import json
from pathlib import Path
from typing import Any

import pytest
from conftest import JudgmentShim

from src.core import event_types as ET
from src.core import spawner
from src.core.config import CharlieBotConfig
from src.core.models import (
  BackendOption,
  SessionMetadata,
  SpawnRequest,
  TaskType,
  ThreadMetadata,
  ThreadStatus,
)

BACKEND_OPTIONS = [
    BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
    BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
    BackendOption(id="kimi-k2.5", label="Kimi", type="cc-kimi", model="kimi-k2.5"),
]


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=BACKEND_OPTIONS,
      model_preference=["claude-opus-4.6", "codex-o3", "kimi-k2.5"],
  )


def _write_events(path: Path, events: list[dict[str, Any]]) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text("".join(json.dumps(event) + "\n" for event in events), encoding="utf-8")


class FakeThreadManager(JudgmentShim):

  def __init__(self, thread: ThreadMetadata, events_path: Path) -> None:
    self.thread = thread
    self.events_path = events_path
    self.saved: list[ThreadMetadata] = []
    self.status_updates: list[tuple[ThreadStatus, int | None]] = []

  async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata:
    assert session_id == self.thread.session_id
    assert thread_id == self.thread.id
    return self.thread

  async def save_metadata(self, thread: ThreadMetadata) -> None:
    self.saved.append(thread.model_copy(deep=True))

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    assert session_id == self.thread.session_id
    assert thread_id == self.thread.id
    return self.events_path

  async def update_status(
      self,
      session_id: str,
      thread_id: str,
      status: ThreadStatus,
      pid: int | None = None,
      exit_code: int | None = None,
      completed_at: Any = None,
  ) -> None:
    del pid
    assert session_id == self.thread.session_id
    assert thread_id == self.thread.id
    self.thread.status = status
    self.thread.exit_code = exit_code
    self.status_updates.append((status, exit_code))


class FakeSessionManager(JudgmentShim):

  def __init__(self, session_id: str, backend: str = "claude-opus-4.6") -> None:
    self.session = SessionMetadata(id=session_id, name="Test", backend=backend)
    self.events: list[dict[str, Any]] = []

  async def get_session(self, session_id: str) -> SessionMetadata:
    assert session_id == self.session.id
    return self.session

  async def mark_unread(self, session_id: str) -> None:
    assert session_id == self.session.id

  async def persist_and_broadcast(self, session_id: str, event: dict[str, Any]) -> None:
    assert session_id == self.session.id
    self.events.append(event)


class _FakeWorker:

  def __init__(
      self,
      thread_metadata: ThreadMetadata,
      working_dir: Path,
      events_log_path: Path,
      task_description: str,
      worker_cfg: CharlieBotConfig,
      backend_option: BackendOption | None = None,
      **kwargs: Any,
  ) -> None:
    del working_dir, events_log_path, task_description, worker_cfg, kwargs
    self.thread_id = thread_metadata.id
    self.backend = backend_option.id


async def _ignore(*args: Any, **kwargs: Any) -> None:
  del args, kwargs


def _install_worker_fakes(
    monkeypatch: pytest.MonkeyPatch,
    worker_runs: list[tuple[str, str]],
    stream_result: spawner._WorkerRunOutcome,
) -> None:
  async def fake_stream_worker_events(worker: _FakeWorker, *args: Any) -> spawner._WorkerRunOutcome:
    del args
    worker_runs.append((worker.thread_id, worker.backend))
    return stream_result

  monkeypatch.setattr(spawner, "Worker", _FakeWorker)
  monkeypatch.setattr(spawner, "_stream_worker_events", fake_stream_worker_events)


@pytest.mark.asyncio
async def test_verify_completion_uses_untruncated_result_without_task_spec_prefix(tmp_path: Path) -> None:
  report = "confirmed | claim | /tmp/source.py:1 | " + "x" * 1200 + "\nRESULT: clean"
  thread = ThreadMetadata(
      id="verify-thread-id",
      session_id="session-id",
      description="## Secret verifier task spec\nFull details must not prefix completion",
      backend="codex-o3",
      model="o3",
      require_review=False,
  )
  thread_mgr = FakeThreadManager(thread, tmp_path / "events.jsonl")
  session_mgr = FakeSessionManager(thread.session_id)

  events_summary, full_summary = await spawner._broadcast_completion(
      thread.session_id,
      thread.description,
      thread,
      spawner._WorkerRunOutcome(exit_code=0, quota_exhausted=False, error=""),
      thread_mgr,
      session_mgr,
      task_type=TaskType.VERIFY,
      verify_report=report,
  )

  assert events_summary == report
  assert report in full_summary
  assert "## Secret verifier task spec" not in full_summary
  assert thread.id in full_summary
  completion = session_mgr.events[-1]
  assert completion["status"] == "completed"
  assert completion["full_content"] == full_summary


@pytest.mark.asyncio
async def test_verify_final_report_falls_back_to_untruncated_last_assistant_message(tmp_path: Path) -> None:
  report = "confirmed | claim | URL | " + "y" * 1200 + "\nRESULT: clean"
  events_path = tmp_path / "events.jsonl"
  _write_events(
      events_path,
      [
          {
              "type": ET.ASSISTANT,
              "message": {
                  "content": [{
                      "type": "text",
                      "text": report
                  }]
              },
          },
          {
              "type": ET.RESULT,
              "result": ""
          },
      ],
  )
  thread = ThreadMetadata(id="verify-thread-id", session_id="session-id", description="Verify")

  result = await spawner.read_verify_final_report(thread.session_id, thread.id, FakeThreadManager(thread, events_path))

  assert result == report


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("report", "worker_error", "expected_status", "expected_exit_code"), [
        ("confirmed | claim | anchor | evidence\nRESULT: clean", "", ThreadStatus.COMPLETED, 0),
        ("mismatch | claim | anchor | evidence\nRESULT: 1 mismatch (0 approval)", "", ThreadStatus.COMPLETED, 0),
        ("mismatch | claim | anchor | evidence\nRESULT: 3 mismatches (2 approval)", "", ThreadStatus.COMPLETED, 0),
        ("mismatch | claim | anchor | evidence\nRESULT: 1 mismatches (1 approval)", "", ThreadStatus.COMPLETED, 0),
        ("", "", ThreadStatus.FAILED, -1),
        ("confirmed | claim | anchor | evidence\nRESULT: clean-ish", "", ThreadStatus.FAILED, -1),
        ("confirmed | claim | anchor | evidence\nRESULT: clean\n\n", "", ThreadStatus.COMPLETED, 0),
        ("confirmed | claim | anchor | evidence\nRESULT: clean-ish", "backend shutdown", ThreadStatus.FAILED, -1),
        ("mismatch | claim | anchor | evidence\nRESULT: 1 mismatch", "", ThreadStatus.FAILED, -1),
        ("mismatch | claim | anchor | evidence\nRESULT: 3 mismatches", "", ThreadStatus.FAILED, -1),
        ("mismatch | claim | anchor | evidence\nRESULT: 2 mismatches (1 approval); CONTRACT-TOUCHING", "", ThreadStatus.FAILED, -1),
        ('{"status": "clean", "mismatches": []}', "", ThreadStatus.FAILED, -1),
    ])
async def test_verify_result_trailer_controls_completion_without_retry(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    report: str,
    worker_error: str,
    expected_status: ThreadStatus,
    expected_exit_code: int,
) -> None:
  cfg = _build_cfg(tmp_path)
  events_path = tmp_path / "events.jsonl"
  _write_events(events_path, [{"type": ET.RESULT, "result": report}])
  thread = ThreadMetadata(id="verify-thread-id", session_id="session-id", description="## Verify plan")
  thread_mgr = FakeThreadManager(thread, events_path)
  session_mgr = FakeSessionManager(thread.session_id)
  worker_runs: list[tuple[str, str]] = []
  _install_worker_fakes(
      monkeypatch, worker_runs, spawner._WorkerRunOutcome(exit_code=0, quota_exhausted=False, error=worker_error))
  monkeypatch.setattr(spawner.review, "maybe_spawn_reviewer", _ignore)

  await spawner.spawn_worker(
      thread.session_id,
      thread.description,
      thread.id,
      cfg,
      session_mgr,
      thread_mgr,
      request=SpawnRequest(
          resolved_backend="codex-o3",
          resolved_model="o3",
          task_type=TaskType.VERIFY,
      ),
  )

  assert worker_runs == [("verify-thread-id", "codex-o3")]
  assert thread.status == expected_status
  assert thread.exit_code == expected_exit_code
  completion = session_mgr.events[-1]
  assert completion["type"] == ET.WORKER_SUMMARY
  assert completion["status"] == ("completed" if expected_status == ThreadStatus.COMPLETED else "failed")
  if expected_status == ThreadStatus.FAILED:
    assert "Verifier completion failed" in completion["full_content"]
  else:
    assert "Verifier completion failed" not in completion["full_content"]
  assert "RESULT:" in completion["full_content"]
  if worker_error:
    assert worker_error in completion["full_content"]
    assert "missing or malformed `RESULT:` trailer" in completion["full_content"]
  assert thread.description not in completion["full_content"]


@pytest.mark.asyncio
async def test_verify_quota_exhaustion_retries_once_with_next_backend_in_same_thread(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = _build_cfg(tmp_path)
  thread = ThreadMetadata(id="verify-thread-id", session_id="session-id", description="Verify")
  thread_mgr = FakeThreadManager(thread, tmp_path / "events.jsonl")
  session_mgr = FakeSessionManager(thread.session_id)
  worker_runs: list[tuple[str, str]] = []
  finalized: dict[str, Any] = {}
  _install_worker_fakes(
      monkeypatch, worker_runs, spawner._WorkerRunOutcome(exit_code=-1, quota_exhausted=True, error=""))

  async def fake_finalize_worker_safely(
      session_id: str,
      description: str,
      thread_meta: ThreadMetadata,
      outcome: spawner._WorkerRunOutcome,
      manager: Any,
      sessions: Any,
      worker_cfg: CharlieBotConfig,
      skip_notify: bool,
      task_type: TaskType = TaskType.IMPLEMENT,
  ) -> None:
    del session_id, description, manager, sessions, worker_cfg, skip_notify
    finalized.update(
        thread=thread_meta,
        exit_code=outcome.exit_code,
        quota_exhausted=outcome.quota_exhausted,
        error=outcome.error,
        task_type=task_type,
    )

  monkeypatch.setattr(spawner, "_finalize_worker_safely", fake_finalize_worker_safely)

  await spawner.spawn_worker(
      thread.session_id,
      thread.description,
      thread.id,
      cfg,
      session_mgr,
      thread_mgr,
      request=SpawnRequest(
          resolved_backend="codex-o3",
          resolved_model="o3",
          task_type=TaskType.VERIFY,
      ),
  )

  assert worker_runs == [(thread.id, "codex-o3"), (thread.id, "kimi-k2.5")]
  assert thread.backend == "kimi-k2.5"
  assert thread.model == "kimi-k2.5"
  assert thread.tried_backends == ["codex-o3", "kimi-k2.5"]
  assert finalized["quota_exhausted"] is True
  assert finalized["task_type"] == TaskType.VERIFY


@pytest.mark.asyncio
async def test_implement_quota_exhaustion_does_not_retry(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _build_cfg(tmp_path)
  thread = ThreadMetadata(id="implement-thread-id", session_id="session-id", description="Implement")
  thread_mgr = FakeThreadManager(thread, tmp_path / "events.jsonl")
  session_mgr = FakeSessionManager(thread.session_id)
  worker_runs: list[tuple[str, str]] = []
  _install_worker_fakes(
      monkeypatch, worker_runs, spawner._WorkerRunOutcome(exit_code=-1, quota_exhausted=True, error=""))
  monkeypatch.setattr(spawner, "_finalize_worker_safely", _ignore)

  await spawner.spawn_worker(
      thread.session_id,
      thread.description,
      thread.id,
      cfg,
      session_mgr,
      thread_mgr,
      request=SpawnRequest(
          resolved_backend="codex-o3",
          resolved_model="o3",
          task_type=TaskType.IMPLEMENT,
      ),
  )

  assert worker_runs == [("implement-thread-id", "codex-o3")]
