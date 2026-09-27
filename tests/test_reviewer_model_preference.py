"""Tests for cross-backend reviewer selection via backends.preference and retry logic."""

from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

import pytest
from conftest import (
    AGY_BACKEND_OPTION,
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
    JudgmentShim,
    ReviewSpawnSessionManager,
    ReviewSpawnThreadManager,
    patch_review_spawn_path,
)
from conftest import THREE_BACKEND_OPTIONS as BACKEND_OPTIONS

from src.core import review, spawner
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption, SessionMetadata, ThreadMetadata

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


def _build_cfg(
    *,
    options: list[BackendOption] | None = None,
    preference: list[str] | None = None,
) -> CharlieBotConfig:
  """A config rooted at /tmp whose worktrees live in the module-scoped dir; backends.options
  defaults to the conftest trio, overridable with *options*, and *preference* sets
  backends.preference when non-empty."""
  backends: dict[str, Any] = {"options": BACKEND_OPTIONS if options is None else options}
  if preference:
    backends["preference"] = preference
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": _WORKTREE_DIR},
      backends=backends,
  )


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




# --- review.spawn_review_worker preference tests ---


@pytest.mark.asyncio
async def test_spawn_review_worker_replaces_failed_reviewer_via_exclusion(monkeypatch: pytest.MonkeyPatch) -> None:
  """On the retry path the failed reviewer itself must not block its replacement."""
  cfg = _build_cfg(preference=["kimi-k2.5"])
  original = _make_original_thread()
  failed_reviewer = ThreadMetadata(
      id="failed-review", session_id="session-id", description="Review", review_of=original.id)
  captured: dict[str, Any] = {}

  class ThreadMgrWithFailedReviewer(ReviewSpawnThreadManager):

    async def list_threads(self, session_id: str) -> list[ThreadMetadata]:
      return [original, failed_reviewer]

  patch_review_spawn_path(monkeypatch, captured)

  spawned = await review.spawn_review_worker(
      "session-id",
      original,
      cfg,
      ReviewSpawnSessionManager("Test"),
      ThreadMgrWithFailedReviewer(),
      exclude_thread_id="failed-review")

  assert spawned is True
  assert captured["request"].resolved_backend == "kimi-k2.5"


# One backends.preference selection rule per case. Row shape: (extra backend option,
# preference, worker backend/model, expected reviewer backend/model).
_PREFERENCE_CASES = [
    pytest.param(None, [], ("codex-o3", "o3"), ("codex-o3", "o3"), id="empty-preference-uses-worker-backend"),
    pytest.param(
        None, ["kimi-k2.5", OPUS_BACKEND_ID], ("codex-o3", "o3"), ("kimi-k2.5", "kimi-k2.5"),
        id="selects-first-non-matching-entry"),
    pytest.param(
        AGY_BACKEND_OPTION, ["agy"], ("codex-o3", "o3"), ("agy", None), id="selects-antigravity-entry-without-model"),
    pytest.param(
        None, ["codex-o3", OPUS_BACKEND_ID], ("codex-o3", "o3"), (OPUS_BACKEND_ID, OPUS_BACKEND_OPTION.model),
        id="skips-entry-matching-worker-backend"),
    pytest.param(
        None, ["nonexistent-1", "nonexistent-2"], ("codex-o3", "o3"), ("codex-o3", "o3"),
        id="invalid-entries-fall-back-to-worker-backend"),
    pytest.param(
        None, ["codex-o3"], ("codex-o3", "o3"), ("codex-o3", "o3"), id="all-entries-matching-worker-fall-back"),
    pytest.param(
        AGY_BACKEND_OPTION, [], ("agy", None), ("agy", None), id="antigravity-worker-missing-model-keeps-backend"),
    pytest.param(
        None, ["nonexistent", "kimi-k2.5"], ("codex-o3", "o3"), ("kimi-k2.5", "kimi-k2.5"),
        id="skips-invalid-entry-selects-next-valid"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("extra_option", "preference", "worker", "expected"), _PREFERENCE_CASES)
async def test_spawn_review_worker_resolves_preference(
    monkeypatch: pytest.MonkeyPatch,
    extra_option: BackendOption | None,
    preference: list[str],
    worker: tuple[str, str | None],
    expected: tuple[str, str | None],
) -> None:
  """spawn_review_worker resolves the reviewer backend/model from backends.preference."""
  cfg = _build_cfg(
      options=[*BACKEND_OPTIONS, extra_option] if extra_option is not None else None,
      preference=preference,
  )
  captured: dict[str, Any] = {}

  patch_review_spawn_path(monkeypatch, captured)

  await review.spawn_review_worker(
      "session-id", _make_original_thread(backend=worker[0], model=worker[1]), cfg, ReviewSpawnSessionManager("Test"),
      ReviewSpawnThreadManager())

  assert captured["request"].resolved_backend == expected[0]
  assert captured["request"].resolved_model == expected[1]


# --- Retry flow tests for review.spawn_review_worker with tried_backends ---


@pytest.mark.asyncio
async def test_retry_skips_tried_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  """On retry, tried_backends are skipped; next untried preference is selected."""
  cfg = _build_cfg(preference=["kimi-k2.5", OPUS_BACKEND_ID])
  captured: dict[str, Any] = {}

  patch_review_spawn_path(monkeypatch, captured)

  result = await review.spawn_review_worker(
      "session-id",
      _make_original_thread(backend="codex-o3", model="o3"),
      cfg,
      ReviewSpawnSessionManager("Test"),
      ReviewSpawnThreadManager(),
      tried_backends=["kimi-k2.5"],
  )

  assert result is True
  assert captured["request"].resolved_backend == OPUS_BACKEND_ID
  assert captured["request"].resolved_model == OPUS_BACKEND_OPTION.model


@pytest.mark.asyncio
async def test_retry_all_prefs_exhausted_falls_back_to_worker(monkeypatch: pytest.MonkeyPatch) -> None:
  """When all preferences are tried, falls back to worker's original backend."""
  cfg = _build_cfg(preference=["kimi-k2.5", OPUS_BACKEND_ID])
  captured: dict[str, Any] = {}

  patch_review_spawn_path(monkeypatch, captured)

  result = await review.spawn_review_worker(
      "session-id",
      _make_original_thread(backend="codex-o3", model="o3"),
      cfg,
      ReviewSpawnSessionManager("Test"),
      ReviewSpawnThreadManager(),
      tried_backends=["kimi-k2.5", OPUS_BACKEND_ID],
  )

  assert result is True
  assert captured["request"].resolved_backend == "codex-o3"
  assert captured["request"].resolved_model == "o3"


def _make_fake_spawn_review(spawn_calls: list[dict], result: bool) -> Callable[..., Awaitable[bool]]:
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

  async def fake_trigger(_session_id: str, summary: str, _cfg: Any, _sm: Any, _etype: str) -> None:
    trigger_calls.append(summary)

  return fake_trigger


# --- review.maybe_spawn_reviewer retry tests ---


async def _fake_read_events_summary(
    session_id: str,
    thread_id: str,
    thread_mgr: Any,
) -> str:
  return "(test events)"


def _make_review_thread(tried_backends: list[str]) -> ThreadMetadata:
  return ThreadMetadata(
      id="review-thread-id",
      session_id="session-id",
      description="Review: Do work",
      review_of="origin-thread-id",
      backend="kimi-k2.5",
      model="kimi-k2.5",
      tried_backends=tried_backends,
      branch_name="charliebot/task-1",
      repo_path="/tmp/repo",
      worktree_path=_WORKTREE_PATH,
  )


class NotifyFakeSessionManager(JudgmentShim):

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    return SessionMetadata(id=session_id, name="Test", backend=OPUS_BACKEND_ID)

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


async def _run_notify_rig(
    monkeypatch: pytest.MonkeyPatch,
    thread: ThreadMetadata,
    *,
    exit_code: int,
    spawn_result: bool,
) -> tuple[list[dict], list[str]]:
  """Run review.maybe_spawn_reviewer against the notify fakes; return (spawn_calls, trigger_calls).

  The thread manager serves *thread* plus the origin thread when the thread carries
  ``review_of`` — the pair the notify path re-reads.
  """
  thread_map: dict[str, ThreadMetadata] = {thread.id: thread}
  if thread.review_of:
    thread_map[thread.review_of] = _make_original_thread()

  spawn_calls: list[dict] = []
  trigger_calls: list[str] = []

  monkeypatch.setattr(review, "spawn_review_worker", _make_fake_spawn_review(spawn_calls, result=spawn_result))
  monkeypatch.setattr(review, "trigger_master", _make_fake_trigger(trigger_calls))
  monkeypatch.setattr(spawner, "read_events_summary", _fake_read_events_summary)

  await review.maybe_spawn_reviewer(
      "session-id",
      thread,
      exit_code,
      "(events summary)",
      "(full summary)",
      NotifyFakeThreadManager(thread_map),
      NotifyFakeSessionManager(),
      _build_cfg(preference=["kimi-k2.5", OPUS_BACKEND_ID]),
  )
  return spawn_calls, trigger_calls


@pytest.mark.asyncio
async def test_notify_retries_exhausted_triggers_master(monkeypatch: pytest.MonkeyPatch) -> None:
  """When all retries are exhausted, trigger master instead of retrying."""
  review_thread = _make_review_thread(tried_backends=["kimi-k2.5", OPUS_BACKEND_ID, "codex-o3"])

  spawn_calls, trigger_calls = await _run_notify_rig(monkeypatch, review_thread, exit_code=1, spawn_result=False)

  assert len(spawn_calls) == 1
  assert len(trigger_calls) == 1
