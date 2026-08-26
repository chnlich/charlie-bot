"""Tests for cross-backend reviewer selection via model_preference and retry logic."""

from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

import pytest
from conftest import THREE_BACKEND_OPTIONS as BACKEND_OPTIONS
from conftest import JudgmentShim

from src.core import review, spawner
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata, SpawnRequest, ThreadMetadata

_WORKTREE_DIR: str = ""
_WORKTREE_PATH: str = ""


@pytest.fixture(scope="module", autouse=True)
def _worktree_paths(tmp_path_factory: pytest.TempPathFactory) -> None:
  """Create a real worktree dir under pytest's tmp area so spawn_review_worker's
  worktree existence check passes (replaces the former hardcoded worktree literals)."""
  global _WORKTREE_DIR, _WORKTREE_PATH
  worktree_root = tmp_path_factory.mktemp("worktrees")
  worktree_subdir = worktree_root / "charliebot-task-1"
  worktree_subdir.mkdir()
  _WORKTREE_DIR = str(worktree_root)
  _WORKTREE_PATH = str(worktree_subdir)


def _build_cfg(**overrides: Any) -> CharlieBotConfig:
  defaults = dict(
      charliebot_home=Path("/tmp/charliebot-test"),
      worktree_dir=_WORKTREE_DIR,
      backend_options=BACKEND_OPTIONS,
  )
  defaults.update(overrides)
  return CharlieBotConfig(**defaults)


def _make_original_thread(
    backend: str = "codex-o3",
    model: str | None = "o3",
) -> ThreadMetadata:
  return ThreadMetadata(
      id="origin-thread-id",
      session_id="session-id",
      description="Do work",
      branch_name="charliebot/task-1",
      base_branch="main",
      repo_path="/tmp/repo",
      worktree_path=_WORKTREE_PATH,
      backend=backend,
      model=model,
  )


class FakeSessionManager(JudgmentShim):

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    return SessionMetadata(id=session_id, name="Test", backend="claude-opus-4.6")


class FakeThreadManager(JudgmentShim):

  def __init__(self) -> None:
    self.saved: list[ThreadMetadata] = []

  async def create_thread(
      self,
      session_meta: SessionMetadata,
      description: str,
      branch_name: str | None = None,
      review_of: str | None = None,
      require_review: bool = True,
  ) -> ThreadMetadata:
    return ThreadMetadata(
        id="review-thread-id",
        session_id=session_meta.id,
        description=description,
        branch_name=branch_name,
        review_of=review_of,
    )

  async def save_metadata(self, meta: ThreadMetadata) -> None:
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
    request: SpawnRequest | None = None,
) -> None:
  return None


def _capture_create_logged_task(captured: dict[str, Any]):
  """Return a fake create_logged_task that captures spawn_worker kwargs."""

  def fake_create_logged_task(coro: Any, *, name: str | None = None) -> Any:
    if coro.cr_frame is not None:
      captured.update(coro.cr_frame.f_locals)
    coro.close()

    class DummyTask:

      def add_done_callback(self, cb: Any) -> None:
        pass

    return DummyTask()

  return fake_create_logged_task


def _patch_reviewer_spawn_path(monkeypatch: pytest.MonkeyPatch, captured: dict[str, Any]) -> None:
  """Patch all three spawn-path seams; an unpatched one shells out to git or forks a backend."""
  monkeypatch.setattr(review, "git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(review, "create_logged_task", _capture_create_logged_task(captured))


# --- review._resolve_preference_option tests ---


def test_resolve_preference_option_valid() -> None:
  cfg = _build_cfg()
  opt = review._resolve_preference_option(cfg, "kimi-k2.5")
  assert opt.id == "kimi-k2.5"
  assert opt.model == "kimi-k2.5"


def test_resolve_preference_option_missing_id() -> None:
  cfg = _build_cfg()
  with pytest.raises(ValueError, match="not in backend_options"):
    review._resolve_preference_option(cfg, "nonexistent")


def test_resolve_preference_option_no_model() -> None:
  cfg = _build_cfg(backend_options=[
      BackendOption(id="no-model", label="No Model", type="cc-claude", model=None),
  ])
  with pytest.raises(ValueError, match="no default model"):
    review._resolve_preference_option(cfg, "no-model")


def test_resolve_preference_option_antigravity_missing_model() -> None:
  cfg = _build_cfg(backend_options=[
      BackendOption(id="agy", label="Antigravity", type="antigravity"),
  ])
  opt = review._resolve_preference_option(cfg, "agy")
  assert opt.id == "agy"
  assert opt.model is None


# --- review.spawn_review_worker preference tests ---


@pytest.mark.asyncio
async def test_spawn_review_worker_skips_when_reviewer_already_exists(monkeypatch: pytest.MonkeyPatch) -> None:
  """Idempotency judgment: a second reviewer for the same original is never derived."""
  cfg = _build_cfg()
  original = _make_original_thread()
  existing_reviewer = ThreadMetadata(
      id="existing-review", session_id="session-id", description="Review", review_of=original.id)
  captured: dict[str, Any] = {}

  class ThreadMgrWithReviewer(FakeThreadManager):

    async def list_threads(self, session_id: str) -> list[ThreadMetadata]:
      return [original, existing_reviewer]

    async def create_thread(self, *args: Any, **kwargs: Any) -> ThreadMetadata:
      raise AssertionError("a reviewer already exists — create_thread must not run")

  monkeypatch.setattr(review, "create_logged_task", _capture_create_logged_task(captured))

  spawned = await review.spawn_review_worker("session-id", original, cfg, FakeSessionManager(), ThreadMgrWithReviewer())

  assert spawned is True
  assert captured == {}  # no spawn task was scheduled


@pytest.mark.asyncio
async def test_spawn_review_worker_replaces_failed_reviewer_via_exclusion(monkeypatch: pytest.MonkeyPatch) -> None:
  """On the retry path the failed reviewer itself must not block its replacement."""
  cfg = _build_cfg(model_preference=["kimi-k2.5"])
  original = _make_original_thread()
  failed_reviewer = ThreadMetadata(
      id="failed-review", session_id="session-id", description="Review", review_of=original.id)
  captured: dict[str, Any] = {}

  class ThreadMgrWithFailedReviewer(FakeThreadManager):

    async def list_threads(self, session_id: str) -> list[ThreadMetadata]:
      return [original, failed_reviewer]

  _patch_reviewer_spawn_path(monkeypatch, captured)

  spawned = await review.spawn_review_worker(
      "session-id", original, cfg, FakeSessionManager(), ThreadMgrWithFailedReviewer(),
      exclude_thread_id="failed-review")

  assert spawned is True
  assert captured["request"].resolved_backend == "kimi-k2.5"


@pytest.mark.asyncio
async def test_empty_preference_uses_worker_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """Empty model_preference -> reviewer uses same backend as worker."""
  cfg = _build_cfg(model_preference=[])
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(), cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["request"].resolved_backend == "codex-o3"
  assert captured["request"].resolved_model == "o3"


@pytest.mark.asyncio
async def test_preference_selects_different_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """First non-matching preference entry is selected for the reviewer."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"), cfg, FakeSessionManager(),
      FakeThreadManager())

  assert captured["request"].resolved_backend == "kimi-k2.5"
  assert captured["request"].resolved_model == "kimi-k2.5"


@pytest.mark.asyncio
async def test_preference_selects_antigravity_missing_model(monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _build_cfg(
      backend_options=BACKEND_OPTIONS + [
          BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
      model_preference=["agy"],
  )
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"), cfg, FakeSessionManager(),
      FakeThreadManager())

  assert captured["request"].resolved_backend == "agy"
  assert captured["request"].resolved_model is None


@pytest.mark.asyncio
async def test_preference_skips_same_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """Entry matching the worker's backend is skipped; next entry is used."""
  cfg = _build_cfg(model_preference=["codex-o3", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"), cfg, FakeSessionManager(),
      FakeThreadManager())

  assert captured["request"].resolved_backend == "claude-opus-4.6"
  assert captured["request"].resolved_model == "claude-opus-4-6"


@pytest.mark.asyncio
async def test_preference_skips_invalid_falls_back(monkeypatch: pytest.MonkeyPatch) -> None:
  """Invalid preference entries are skipped; falls back to worker backend."""
  cfg = _build_cfg(model_preference=["nonexistent-1", "nonexistent-2"])
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(), cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["request"].resolved_backend == "codex-o3"
  assert captured["request"].resolved_model == "o3"


@pytest.mark.asyncio
async def test_preference_all_same_as_worker_falls_back(monkeypatch: pytest.MonkeyPatch) -> None:
  """All preference entries match worker backend -> falls back."""
  cfg = _build_cfg(model_preference=["codex-o3"])
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"), cfg, FakeSessionManager(),
      FakeThreadManager())

  assert captured["request"].resolved_backend == "codex-o3"
  assert captured["request"].resolved_model == "o3"


@pytest.mark.asyncio
async def test_antigravity_worker_missing_model_falls_back_to_same_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _build_cfg(
      backend_options=BACKEND_OPTIONS + [
          BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
      model_preference=[],
  )
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(backend="agy", model=None), cfg, FakeSessionManager(), FakeThreadManager())

  assert captured["request"].resolved_backend == "agy"
  assert captured["request"].resolved_model is None


@pytest.mark.asyncio
async def test_preference_skips_invalid_then_selects_valid(monkeypatch: pytest.MonkeyPatch) -> None:
  """Invalid entry skipped, next valid entry selected."""
  cfg = _build_cfg(model_preference=["nonexistent", "kimi-k2.5"])
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(backend="codex-o3", model="o3"), cfg, FakeSessionManager(),
      FakeThreadManager())

  assert captured["request"].resolved_backend == "kimi-k2.5"
  assert captured["request"].resolved_model == "kimi-k2.5"


@pytest.mark.asyncio
async def test_spawn_review_worker_returns_false_when_session_missing(monkeypatch: pytest.MonkeyPatch) -> None:
  """A missing session must not dereference a None session_meta.

  ``spawn_review_worker`` returns False and creates no review thread when
  ``get_session`` returns None, mirroring the ``ctx is None`` exit.
  """
  cfg = _build_cfg()
  original = _make_original_thread()

  class MissingSessionManager(FakeSessionManager):

    async def get_session(self, session_id: str) -> SessionMetadata | None:
      return None

  class ThreadMgrNoCreate(FakeThreadManager):

    async def create_thread(self, *args: Any, **kwargs: Any) -> ThreadMetadata:
      raise AssertionError("create_thread must not run when the session is missing")

  spawned = await review.spawn_review_worker(
      "session-id", original, cfg, MissingSessionManager(), ThreadMgrNoCreate())

  assert spawned is False


# --- Retry flow tests for review.spawn_review_worker with tried_backends ---


@pytest.mark.asyncio
async def test_retry_skips_tried_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """On retry, tried_backends are skipped; next untried preference is selected."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  result = await review.spawn_review_worker(
      "session-id",
      _make_original_thread(backend="codex-o3", model="o3"),
      cfg,
      FakeSessionManager(),
      FakeThreadManager(),
      tried_backends=["kimi-k2.5"],
  )

  assert result is True
  assert captured["request"].resolved_backend == "claude-opus-4.6"
  assert captured["request"].resolved_model == "claude-opus-4-6"


@pytest.mark.asyncio
async def test_retry_all_prefs_exhausted_falls_back_to_worker(monkeypatch: pytest.MonkeyPatch) -> None:
  """When all preferences are tried, falls back to worker's original backend."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  captured: dict[str, Any] = {}

  _patch_reviewer_spawn_path(monkeypatch, captured)

  result = await review.spawn_review_worker(
      "session-id",
      _make_original_thread(backend="codex-o3", model="o3"),
      cfg,
      FakeSessionManager(),
      FakeThreadManager(),
      tried_backends=["kimi-k2.5", "claude-opus-4.6"],
  )

  assert result is True
  assert captured["request"].resolved_backend == "codex-o3"
  assert captured["request"].resolved_model == "o3"


@pytest.mark.asyncio
async def test_retry_all_backends_exhausted_returns_false(monkeypatch: pytest.MonkeyPatch) -> None:
  """When all backends including worker are tried, returns False."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(review, "git_current_branch", _fake_git_current_branch)

  result = await review.spawn_review_worker(
      "session-id",
      _make_original_thread(backend="codex-o3", model="o3"),
      cfg,
      FakeSessionManager(),
      FakeThreadManager(),
      tried_backends=["kimi-k2.5", "claude-opus-4.6", "codex-o3"],
  )

  assert result is False


@pytest.mark.asyncio
async def test_tried_backends_propagated_to_review_thread(monkeypatch: pytest.MonkeyPatch) -> None:
  """Review thread metadata gets tried_backends set."""
  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])
  thread_mgr = FakeThreadManager()

  monkeypatch.setattr(review, "git_current_branch", _fake_git_current_branch)
  monkeypatch.setattr(spawner, "spawn_worker", _fake_spawn_worker)
  monkeypatch.setattr(review, "create_logged_task", _capture_create_logged_task({}))

  await review.spawn_review_worker(
      "session-id",
      _make_original_thread(backend="codex-o3", model="o3"),
      cfg,
      FakeSessionManager(),
      thread_mgr,
      tried_backends=["kimi-k2.5"],
  )

  # The saved review thread should have tried_backends = ["kimi-k2.5", "claude-opus-4.6"]
  saved = [m for m in thread_mgr.saved if m.review_of]
  assert len(saved) == 1
  assert saved[0].tried_backends == ["kimi-k2.5", "claude-opus-4.6"]


def _make_fake_spawn_review(spawn_calls: list[dict], result: bool = True) -> Callable[..., Awaitable[bool]]:
  """A ``review.spawn_review_worker`` stand-in recording the backend preference per call.

  The signature mirrors the production call; each test reads its own ``spawn_calls``.
  """

  async def fake_spawn_review(
      _session_id: str,
      _orig: Any,
      _cfg: Any,
      _sm: Any,
      _tm: Any,
      tried_backends: Any = None,
      exclude_thread_id: Any = None,
  ) -> bool:
    spawn_calls.append({"tried_backends": tried_backends})
    return result

  return fake_spawn_review


def _make_fake_trigger(trigger_calls: list[str]) -> Callable[..., Awaitable[None]]:
  """A ``review.trigger_master`` stand-in capturing the trigger summary per call."""

  async def fake_trigger(_session_id: str, summary: str, _cfg: Any, _sm: Any) -> None:
    trigger_calls.append(summary)

  return fake_trigger


# --- review.maybe_spawn_reviewer retry tests ---


async def _noop(*args: Any, **kwargs: Any) -> None:
  pass


async def _fake_read_events_summary(
    session_id: str,
    thread_id: str,
    thread_mgr: Any,
) -> str:
  return "(test events)"


def _make_review_thread(tried_backends: list[str] | None = None,) -> ThreadMetadata:
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
      worktree_path=_WORKTREE_PATH,
  )


class NotifyFakeSessionManager(JudgmentShim):

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    return SessionMetadata(id=session_id, name="Test", backend="claude-opus-4.6")

  async def save_metadata(self, meta: Any) -> None:
    pass

  async def mark_unread(self, session_id: str) -> None:
    pass

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    pass

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    pass


class NotifyFakeThreadManager(JudgmentShim):

  def __init__(self, threads: dict[str, ThreadMetadata]) -> None:
    self._threads = threads

  async def get_thread(self, session_id: str, thread_id: str) -> ThreadMetadata | None:
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
  trigger_calls: list[str] = []

  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(review, "spawn_review_worker", _make_fake_spawn_review(spawn_calls))
  monkeypatch.setattr(review, "trigger_master", _make_fake_trigger(trigger_calls))
  monkeypatch.setattr(spawner, "read_events_summary", _fake_read_events_summary)

  await review.maybe_spawn_reviewer(
      "session-id", review_thread, 1, "(events summary)", "(full summary)", thread_mgr, NotifyFakeSessionManager(), cfg)

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

  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(review, "spawn_review_worker", _make_fake_spawn_review(spawn_calls))
  monkeypatch.setattr(review, "trigger_master", _make_fake_trigger(trigger_calls))
  monkeypatch.setattr(spawner, "read_events_summary", _fake_read_events_summary)

  await review.maybe_spawn_reviewer(
      "session-id", review_thread, 0, "(events summary)", "(full summary)", thread_mgr, NotifyFakeSessionManager(), cfg)

  assert len(spawn_calls) == 0
  assert len(trigger_calls) == 1


@pytest.mark.asyncio
async def test_notify_retries_exhausted_triggers_master(monkeypatch: pytest.MonkeyPatch) -> None:
  """When all retries are exhausted, trigger master instead of retrying."""
  review_thread = _make_review_thread(tried_backends=["kimi-k2.5", "claude-opus-4.6", "codex-o3"])
  original_thread = _make_original_thread()

  thread_mgr = NotifyFakeThreadManager({
      "review-thread-id": review_thread,
      "origin-thread-id": original_thread,
  })

  spawn_calls: list[dict] = []
  trigger_calls: list[str] = []

  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(review, "spawn_review_worker", _make_fake_spawn_review(spawn_calls, result=False))
  monkeypatch.setattr(review, "trigger_master", _make_fake_trigger(trigger_calls))
  monkeypatch.setattr(spawner, "read_events_summary", _fake_read_events_summary)

  await review.maybe_spawn_reviewer(
      "session-id", review_thread, 1, "(events summary)", "(full summary)", thread_mgr, NotifyFakeSessionManager(), cfg)

  assert len(spawn_calls) == 1
  assert len(trigger_calls) == 1


# --- require_review gating tests ---


@pytest.mark.asyncio
async def test_require_review_false_skips_reviewer_triggers_master(monkeypatch: pytest.MonkeyPatch) -> None:
  """When require_review=False, no reviewer is spawned and master is triggered directly."""
  worker_thread = ThreadMetadata(
      id="worker-thread-id",
      session_id="session-id",
      description="Prompt task",
      require_review=False,
      backend="claude-opus-4.6",
      model="claude-opus-4-6",
      branch_name="charliebot/task-1",
      repo_path="/tmp/repo",
      worktree_path=_WORKTREE_PATH,
  )

  thread_mgr = NotifyFakeThreadManager({
      "worker-thread-id": worker_thread,
  })

  spawn_calls: list[dict] = []
  trigger_calls: list[str] = []

  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(review, "spawn_review_worker", _make_fake_spawn_review(spawn_calls))
  monkeypatch.setattr(review, "trigger_master", _make_fake_trigger(trigger_calls))
  monkeypatch.setattr(spawner, "read_events_summary", _fake_read_events_summary)

  await review.maybe_spawn_reviewer(
      "session-id", worker_thread, 0, "(events summary)", "(full summary)", thread_mgr, NotifyFakeSessionManager(), cfg)

  # No reviewer spawned
  assert len(spawn_calls) == 0
  # Master triggered directly
  assert len(trigger_calls) == 1


@pytest.mark.asyncio
async def test_require_review_true_spawns_reviewer(monkeypatch: pytest.MonkeyPatch) -> None:
  """When require_review=True (default), reviewer is spawned as usual."""
  worker_thread = ThreadMetadata(
      id="worker-thread-id",
      session_id="session-id",
      description="Implement task",
      require_review=True,
      backend="claude-opus-4.6",
      model="claude-opus-4-6",
      branch_name="charliebot/task-1",
      repo_path="/tmp/repo",
      worktree_path=_WORKTREE_PATH,
  )

  thread_mgr = NotifyFakeThreadManager({
      "worker-thread-id": worker_thread,
  })

  spawn_calls: list[dict] = []
  trigger_calls: list[str] = []

  cfg = _build_cfg(model_preference=["kimi-k2.5", "claude-opus-4.6"])

  monkeypatch.setattr(review, "spawn_review_worker", _make_fake_spawn_review(spawn_calls))
  monkeypatch.setattr(review, "trigger_master", _make_fake_trigger(trigger_calls))
  monkeypatch.setattr(spawner, "read_events_summary", _fake_read_events_summary)

  await review.maybe_spawn_reviewer(
      "session-id", worker_thread, 0, "(events summary)", "(full summary)", thread_mgr, NotifyFakeSessionManager(), cfg)

  # Reviewer spawned
  assert len(spawn_calls) == 1
  # Master NOT triggered directly (reviewer handles that)
  assert len(trigger_calls) == 0
