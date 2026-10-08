"""Regression tests for /api/internal/delegate takeoff gate behavior."""

from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from conftest import (
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
)
from conftest import THREE_BACKEND_OPTIONS as VERIFY_BACKEND_OPTIONS
from fastapi import HTTPException

from src.features.improve import api as improve_api
from src.infra.config import CharlieBotConfig
from src.infra.models import DelegateRequest, SessionMetadata, TaskType
from src.runtime import spawner_backends
from src.runtime.api import internal
from src.runtime.takeoff_gate import DelegationBlockedError


def _stub_task_manager():
  """A task-tree manager with an empty session lookup for tests that stub delegation."""
  from src.runtime.task_sessions import TaskTreeManager
  return TaskTreeManager(CharlieBotConfig(charliebot_home=Path("/tmp/delegate-takeoff-stub")), _LastSessionManager())


class _LastSessionManager:
  """The no-op session seam the stub task-tree owner requires; its store and block attributes are itself."""

  def __init__(self) -> None:
    self.store = self
    self.events = self
    self.sidebar = self

  async def get_session(self, session_id: str):
    return None

  def fresh_cached_metas(self):
    return {}


def _build_request(
    task_type: TaskType = TaskType.IMPLEMENT,
    repo_path: str | None = "/tmp/repo",
    base_branch: str | None = "main",
    backend: str | None = "codex-o3",
) -> DelegateRequest:
  return DelegateRequest(
      session_id="session-id",
      description="Do work",
      base_branch=base_branch,
      backend=backend,
      repo_path=repo_path,
      task_type=task_type,
  )


def _improve_request() -> improve_api.ImproveRequest:
  return improve_api.ImproveRequest(
      session_id="session-id",
      repo_path="/tmp/repo",
      base_branch="main",
      backend="codex-o3",
      goal="Improve this",
  )


def _patch_resolve_rig(monkeypatch: pytest.MonkeyPatch) -> object:
  """Install the resolve/config pair the _authorize_spawn_request tests share.

  Returns the stubbed cfg so each test asserts its resolved_cfg is this object.
  """
  cfg = object()

  async def fake_resolve(*args: Any, **kwargs: Any) -> tuple[str, str]:
    del args, kwargs
    return "codex-o3", "o3"

  monkeypatch.setattr(internal, "get_config", lambda: cfg)
  monkeypatch.setattr(spawner_backends, "resolve_requested_subagent_backend_model", fake_resolve)
  return cfg


@pytest.mark.asyncio
async def test_delegate_task_returns_403_when_takeoff_gate_blocks(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.store.get_session.return_value = SessionMetadata(profile="manager", id=req.session_id, name="Test")
  task_mgr = _stub_task_manager()
  monkeypatch.setattr(task_mgr, "check_task_authorization", AsyncMock(side_effect=DelegationBlockedError("blocked")))

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_events=session_mgr.events, store=session_mgr.store, task_mgr=task_mgr)

  assert exc_info.value.status_code == 403
  assert exc_info.value.detail == "blocked"
  session_mgr.events.persist_and_broadcast.assert_not_awaited()


@pytest.mark.asyncio
async def test_improve_stays_blocked_without_takeoff() -> None:
  req = _improve_request()
  store = AsyncMock()
  store.get_session.return_value = SessionMetadata(profile="manager", id=req.session_id, name="Test")
  task_mgr = _stub_task_manager()
  task_mgr.check_task_authorization = AsyncMock(side_effect=DelegationBlockedError("blocked"))

  with pytest.raises(HTTPException) as exc_info:
    await improve_api.start_improve_loop(
        req, cfg=CharlieBotConfig(charliebot_home=Path("/tmp/improve-stub")), store=store, task_mgr=task_mgr)

  assert exc_info.value.status_code == 403
  assert exc_info.value.detail == "blocked"


@pytest.mark.asyncio
async def test_delegate_task_verify_rejects_repo_path() -> None:
  req = _build_request(task_type=TaskType.VERIFY, repo_path="/tmp/repo", base_branch=None)
  session_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_events=session_mgr.events, store=session_mgr.store)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "verify delegations are repo-less; omit repo_path"
  session_mgr.store.get_session.assert_not_awaited()


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
@pytest.mark.parametrize("half", ["repo", "branch"])
async def test_delegate_task_repo_scoped_types_take_repo_and_base_together(task_type: TaskType, half: str) -> None:
  """Exactly one of repo_path/base_branch is a malformed repo-less delegation."""
  repo_path = "/tmp/repo" if half == "repo" else None
  base_branch = "main" if half == "branch" else None
  req = _build_request(task_type=task_type, repo_path=repo_path, base_branch=base_branch)
  session_mgr = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_events=session_mgr.events, store=session_mgr.store)

  assert exc_info.value.status_code == 400
  assert "together" in exc_info.value.detail
  session_mgr.store.get_session.assert_not_awaited()


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_delegate_task_repo_less_request_passes_the_schema_gate(
    task_type: TaskType, monkeypatch: pytest.MonkeyPatch) -> None:
  """Omitting repo_path and base_branch together is a valid repo-less delegation."""
  req = _build_request(task_type=task_type, repo_path=None, base_branch=None)
  session_mgr = AsyncMock()
  session_mgr.store.get_session.return_value = SessionMetadata(profile="manager", id=req.session_id, name="Test")
  _patch_resolve_rig(monkeypatch)
  task_mgr = _stub_task_manager()
  task_mgr.check_task_authorization = AsyncMock()
  captured: dict = {}

  async def fake_delegate_task_tree(req, task_mgr, session_mgr, caller, backend, model):
    captured["repo_path"] = req.repo_path
    captured["base_branch"] = req.base_branch
    return {
        "session_id": "child",
        "parent_session_id": req.session_id,
        "run_id": "r",
        "thread_id": "r",
        "description": req.description
    }

  monkeypatch.setattr(internal, "_delegate_task_tree", fake_delegate_task_tree)

  result = await internal.delegate_task(
      req, session_events=session_mgr.events, store=session_mgr.store, task_mgr=task_mgr, caller=None)

  assert result["session_id"] == "child"
  assert captured["repo_path"] is None
  assert captured["base_branch"] is None


@pytest.mark.asyncio
async def test_delegate_task_returns_400_for_invalid_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_mgr = AsyncMock()
  session_mgr.store.get_session.return_value = SessionMetadata(profile="manager", id=req.session_id, name="Test")
  task_mgr = _stub_task_manager()
  task_mgr.check_task_authorization = AsyncMock()

  async def fake_resolve_requested_subagent_backend_model(*args: Any, **kwargs: Any) -> tuple[str, str]:
    raise ValueError("requested backend 'codex-o3' is not in backends.options")

  monkeypatch.setattr(
      spawner_backends, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "get_config", lambda: object())

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_events=session_mgr.events, store=session_mgr.store, task_mgr=task_mgr)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'codex-o3' is not in backends.options"


# --- verify default backend via backends.preference ---


def _build_verify_cfg(preference: list[str]) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options": VERIFY_BACKEND_OPTIONS,
          "preference": preference
      },
  )


class BackendFakeSessionStore:

  def __init__(self, backend: str) -> None:
    self.backend = backend

  async def get_session(self, session_id: str) -> SessionMetadata:
    return SessionMetadata(profile="manager", id=session_id, name="Test", backend=self.backend)


async def _authorize_verify(
    monkeypatch: pytest.MonkeyPatch,
    session_backend: str,
    preference: list[str],
    backend: str | None = None,
) -> tuple[str | None, str | None]:
  req = _build_request(task_type=TaskType.VERIFY, repo_path=None, base_branch=None, backend=backend)
  monkeypatch.setattr(internal, "get_config", lambda: _build_verify_cfg(preference))
  store = BackendFakeSessionStore(session_backend)
  resolved_backend, resolved_model = await internal._authorize_spawn_request(req, store, _stub_task_manager())
  return resolved_backend, resolved_model


@pytest.mark.asyncio
async def test_verify_no_backend_defaults_to_first_differing_preference(monkeypatch: pytest.MonkeyPatch) -> None:
  """Session backend is the first preference entry -> the second (first differing) entry wins."""
  resolved = await _authorize_verify(
      monkeypatch, session_backend=OPUS_BACKEND_ID, preference=[OPUS_BACKEND_ID, "codex-o3"])
  assert resolved == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_verify_no_backend_session_backend_not_in_preference_uses_first_entry(
    monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(monkeypatch, session_backend="kimi-k2.5", preference=[OPUS_BACKEND_ID, "codex-o3"])
  assert resolved == (OPUS_BACKEND_ID, OPUS_BACKEND_OPTION.model)


@pytest.mark.asyncio
async def test_verify_no_backend_empty_preference_keeps_session_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(monkeypatch, session_backend="codex-o3", preference=[])
  assert resolved == ("codex-o3", "o3")


@pytest.mark.asyncio
async def test_verify_explicit_backend_wins_over_preference(monkeypatch: pytest.MonkeyPatch) -> None:
  resolved = await _authorize_verify(
      monkeypatch,
      session_backend=OPUS_BACKEND_ID,
      preference=[OPUS_BACKEND_ID, "codex-o3"],
      backend="kimi-k2.5",
  )
  assert resolved == ("kimi-k2.5", "kimi-k2.5")


@pytest.mark.asyncio
async def test_verify_unknown_explicit_backend_returns_400(monkeypatch: pytest.MonkeyPatch) -> None:
  with pytest.raises(HTTPException) as exc_info:
    await _authorize_verify(
        monkeypatch,
        session_backend=OPUS_BACKEND_ID,
        preference=[OPUS_BACKEND_ID, "codex-o3"],
        backend="nonexistent",
    )

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "requested backend 'nonexistent' is not in backends.options"
