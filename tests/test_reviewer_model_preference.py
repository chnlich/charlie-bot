"""Tests for cross-backend reviewer selection via model_preference and retry logic."""

from pathlib import Path
from typing import Any, Optional

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata, ThreadMetadata
from src.core import spawner


BACKEND_OPTIONS = [
    BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
    BackendOption(id="codex-o3", label="Codex", type="codex", model="o3"),
    BackendOption(id="kimi-k2.5", label="Kimi", type="cc-kimi", model="kimi-k2.5"),
]


def _build_cfg(**overrides: Any) -> CharlieBotConfig:
  defaults = dict(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir="/tmp/worktrees",
      backend_options=BACKEND_OPTIONS,
  )
  defaults.update(overrides)
  return CharlieBotConfig(**defaults)


def _make_original_thread(
    backend: str = "codex-o3",
    model: str = "o3",
) -> ThreadMetadata:
  return ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="Do work",
      branch_name="charliebot/task-1",
      repo_path="/tmp/repo",
      worktree_path="/tmp/worktrees/charliebot-task-1",
      backend=backend,
      model=model,
  )


class FakeSessionManager:
  async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
    return SessionMetadata(id=session_id, name="Test", backend="claude-opus-4.6")


class FakeThreadManager:
  def __init__(self) -> None:
    self.saved: list[ThreadMetadata] = []

  async def create_thread(
      self,
      session_meta: SessionMetadata,
      description: str,
      branch_name: Optional[str] = None,
      review_of: Optional[str] = None,
  ) -> ThreadMetadata:
    return ThreadMetadata(
        id="review-thread-id",
        session_id=session_meta.id,
        description=description,
        branch_name=branch_name,
        review_of=review_of,
    )

  async def _save_metadata(self, meta: ThreadMetadata) -> None:
    self.saved.append(meta)


async def _fake_git_current_branch(repo_path: Path) -> str:
  return "main"


async def _fake_spawn_worker(
    session_id: str,
    description: str,
    thread_id: str,
    cfg: CharlieBotConfig,
    session_mgr: Any,
    thread_mgr: Any,
    repo_path: Optional[str] = None,
    prompt_override: Optional[str] = None,
    resolved_backend: str = "",
    resolved_model: str = "",
) -> None:
  return None


def _capture_create_task(captured: dict[str, Any]):
  """Return a fake asyncio.create_task that captures spawn_worker kwargs."""

  def fake_create_task(coro: Any, name: Optional[str] = None) -> Any:
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()

    class DummyTask:
      pass

    return DummyTask()

  return fake_create_task


# --- resolve_preference_option tests ---


def test_resolve_preference_option_valid() -> None:
  cfg = _build_cfg()
  opt = spawner._resolve_preference_option(cfg, "kimi-k2.5")
  assert opt.id == "kimi-k2.5"
  assert opt.model == "kimi-k2.5"


def test_resolve_preference_option_missing_id() -> None:
  cfg = _build_cfg()
  with pytest.raises(ValueError, match="not in backend_options"):
    spawner._resolve_preference_option(cfg, "nonexistent")


def test_resolve_preference_option_no_model() -> None:
  cfg = _build_cfg(backend_options=[
      BackendOption(id="no-model", label="No Model", type="cc-claude", model=None),
  ])
  with pytest.raises(ValueError, match="no default model"):
    spawner._resolve_preference_option(cfg, "no-model")


# --- _spawn_review_worker preference tests ---


@pytest.mark.asyncio
async def test_empty_preference_uses_worker_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """Empty model_preference -> reviewer uses same backend as worker."""
  cfg = _build_cfg(model_preference=[])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(), cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"


@pytest.mark.asyncio
async def test_preference_selects_different_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """First non-matching preference entry is selected for the reviewer."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "kimi-k2.5"
  assert captured["resolved_model"] == "kimi-k2.5"


@pytest.mark.asyncio
async def test_preference_skips_same_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """Entry matching the worker's backend is skipped; next entry is used."""
  cfg = _build_cfg(model_preference=["codex-o3", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "claude-opus-4.6"
  assert captured["resolved_model"] == "claude-opus-4-6"


@pytest.mark.asyncio
async def test_preference_skips_invalid_falls_back(monkeypatch: pytest.MonkeyPatch) -> None:
  """Invalid preference entries are skipped; falls back to worker backend."""
  cfg = _build_cfg(model_preference=["nonexistent-1", "nonexistent-2"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(), cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"


@pytest.mark.asyncio
async def test_preference_all_same_as_worker_falls_back(monkeypatch: pytest.MonkeyPatch) -> None:
  """All preference entries match worker backend -> falls back."""
  cfg = _build_cfg(model_preference=["codex-o3"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"


@pytest.mark.asyncio
async def test_preference_skips_invalid_then_selects_valid(monkeypatch: pytest.MonkeyPatch) -> None:
  """Invalid entry skipped, next valid entry selected."""
  cfg = _build_cfg(model_preference=["nonexistent", "kimi-k2.5"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["resolved_backend"] == "kimi-k2.5"
  assert captured["resolved_model"] == "kimi-k2.5"


# --- Retry flow tests for _spawn_review_worker with tried_backends ---


@pytest.mark.asyncio
async def test_retry_skips_tried_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """On retry, tried_backends are skipped; next untried preference is selected."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  result = await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager(),
      tried_backends=["kimi-k2.5"],
  )

  assert result is True
  assert captured["resolved_backend"] == "claude-opus-4.6"
  assert captured["resolved_model"] == "claude-opus-4-6"


@pytest.mark.asyncio
async def test_retry_all_prefs_exhausted_falls_back_to_worker(monkeypatch: pytest.MonkeyPatch) -> None:
  """When all preferences are tried, falls back to worker's original backend."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task(captured))

  result = await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager(),
      tried_backends=["kimi-k2.5", "claude-opus-4.6"],
  )

  assert result is True
  assert captured["resolved_backend"] == "codex-o3"
  assert captured["resolved_model"] == "o3"


@pytest.mark.asyncio
async def test_retry_all_backends_exhausted_returns_false(monkeypatch: pytest.MonkeyPatch) -> None:
  """When all backends including worker are tried, returns False."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)

  result = await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), FakeThreadManager(),
      tried_backends=["kimi-k2.5", "claude-opus-4.6", "codex-o3"],
  )

  assert result is False


@pytest.mark.asyncio
async def test_tried_backends_propagated_to_review_thread(monkeypatch: pytest.MonkeyPatch) -> None:
  """Review thread metadata gets tried_backends set."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  thread_mgr = FakeThreadManager()

  monkeypatch.setattr(spawner, "_git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(spawner.asyncio, "create_task", _capture_create_task({}))

  await spawner._spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"),
      cfg, FakeSessionManager(), thread_mgr,
      tried_backends=["kimi-k2.5"],
  )

  # The saved review thread should have tried_backends = ["kimi-k2.5", "claude-opus-4.6"]
  saved = [m for m in thread_mgr.saved if m.review_of]
  assert len(saved) == 1
  assert saved[0].tried_backends == ["kimi-k2.5", "claude-opus-4.6"]


# --- _notify_completion reviewer retry tests ---


async def _noop(*args: Any, **kwargs: Any) -> None:
  pass


async def _fake_read_events_summary(
    session_id: str, thread_id: str, thread_mgr: Any, max_lines: int = 80,
) -> str:
  return "(test events)"


def _make_review_thread(
    tried_backends: Optional[list[str]] = None,
) -> ThreadMetadata:
  return ThreadMetadata(
      id="review-thread-id",
      session_id="session-id",
      description="Review: Do work",
      review_of="origin-thread-id",
      backend="kimi-k2.5",
      model="kimi-k2.5",
      tried_backends=tried_backends or [],
      branch_name="charliebot/task-1",
      repo_path="/tmp/repo",
      worktree_path="/tmp/worktrees/charliebot-task-1",
  )


class NotifyFakeSessionManager:

  async def get_session(self, session_id: str) -> Optional[SessionMetadata]:
    return SessionMetadata(id=session_id, name="Test", backend="claude-opus-4.6")

  async def save_metadata(self, meta: Any) -> None:
    pass

  async def mark_unread(self, session_id: str) -> None:
    pass

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    pass


class NotifyFakeThreadManager:

  def __init__(self, threads: dict[str, ThreadMetadata]) -> None:
    self._threads = threads

  async def get_thread(self, session_id: str, thread_id: str) -> Optional[ThreadMetadata]:
    return self._threads.get(thread_id)

  async def get_events_log_path(self, session_id: str, thread_id: str) -> Path:
    return Path("/tmp/events.jsonl")


@pytest.mark.asyncio
async def test_notify_reviewer_failure_triggers_retry(monkeypatch: pytest.MonkeyPatch) -> None:
  """When a reviewer fails, _notify_completion retries with next backend."""
  review_thread = _make_review_thread(tried_backends=["kimi-k2.5"])
  original_thread = _make_original_thread()

  thread_mgr = NotifyFakeThreadManager({
      "review-thread-id": review_thread,
      "origin-thread-id": original_thread,
  })

  spawn_calls: list[dict] = []
  trigger_calls: list[bool] = []

  async def fake_spawn_review(
      session_id: str, orig: Any, cfg: Any, sm: Any, tm: Any, tried_backends: Any = None,
  ) -> bool:
    spawn_calls.append({"tried_backends": tried_backends})
    return True

  async def fake_trigger(session_id: str, summary: str, cfg: Any, sm: Any) -> None:
    trigger_calls.append(True)

  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(spawner, "_spawn_review_worker", fake_spawn_review)
  monkeypatch.setattr(spawner, "_trigger_master", fake_trigger)
  monkeypatch.setattr(spawner, "broadcast_and_persist", _noop)
  monkeypatch.setattr(spawner, "_read_events_summary", _fake_read_events_summary)

  await spawner._notify_completion(
      "session-id", "Do work", review_thread, 1,
      thread_mgr, NotifyFakeSessionManager(), cfg)

  assert len(spawn_calls) == 1
  assert spawn_calls[0]["tried_backends"] == ["kimi-k2.5"]
  assert len(trigger_calls) == 0


@pytest.mark.asyncio
async def test_notify_reviewer_success_no_retry(monkeypatch: pytest.MonkeyPatch) -> None:
  """When a reviewer succeeds, no retry; trigger master directly."""
  review_thread = _make_review_thread(tried_backends=["kimi-k2.5"])
  original_thread = _make_original_thread()

  thread_mgr = NotifyFakeThreadManager({
      "review-thread-id": review_thread,
      "origin-thread-id": original_thread,
  })

  spawn_calls: list[dict] = []
  trigger_calls: list[str] = []

  async def fake_spawn_review(
      session_id: str, orig: Any, cfg: Any, sm: Any, tm: Any, tried_backends: Any = None,
  ) -> bool:
    spawn_calls.append({"tried_backends": tried_backends})
    return True

  async def fake_trigger(session_id: str, summary: str, cfg: Any, sm: Any) -> None:
    trigger_calls.append(summary)

  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(spawner, "_spawn_review_worker", fake_spawn_review)
  monkeypatch.setattr(spawner, "_trigger_master", fake_trigger)
  monkeypatch.setattr(spawner, "broadcast_and_persist", _noop)
  monkeypatch.setattr(spawner, "_read_events_summary", _fake_read_events_summary)

  await spawner._notify_completion(
      "session-id", "Do work", review_thread, 0,
      thread_mgr, NotifyFakeSessionManager(), cfg)

  assert len(spawn_calls) == 0
  assert len(trigger_calls) == 1


@pytest.mark.asyncio
async def test_notify_retries_exhausted_triggers_master(monkeypatch: pytest.MonkeyPatch) -> None:
  """When all retries are exhausted, trigger master instead of retrying."""
  review_thread = _make_review_thread(
      tried_backends=["kimi-k2.5", "claude-opus-4.6", "codex-o3"])
  original_thread = _make_original_thread()

  thread_mgr = NotifyFakeThreadManager({
      "review-thread-id": review_thread,
      "origin-thread-id": original_thread,
  })

  spawn_calls: list[dict] = []
  trigger_calls: list[str] = []

  async def fake_spawn_review(
      session_id: str, orig: Any, cfg: Any, sm: Any, tm: Any, tried_backends: Any = None,
  ) -> bool:
    spawn_calls.append({"tried_backends": tried_backends})
    return False

  async def fake_trigger(session_id: str, summary: str, cfg: Any, sm: Any) -> None:
    trigger_calls.append(summary)

  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(spawner, "_spawn_review_worker", fake_spawn_review)
  monkeypatch.setattr(spawner, "_trigger_master", fake_trigger)
  monkeypatch.setattr(spawner, "broadcast_and_persist", _noop)
  monkeypatch.setattr(spawner, "_read_events_summary", _fake_read_events_summary)

  await spawner._notify_completion(
      "session-id", "Do work", review_thread, 1,
      thread_mgr, NotifyFakeSessionManager(), cfg)

  assert len(spawn_calls) == 1
  assert len(trigger_calls) == 1
