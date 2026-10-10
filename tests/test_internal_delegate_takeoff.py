"""Regression tests for the /api/internal delegate and improve entries: the takeoff gate,
and the worker child record carrying the backend its creator resolved.
"""

from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from conftest import (
    OPERATOR,
    OPUS_BACKEND_ID,
    OPUS_BACKEND_OPTION,
    SessionBlocks,
    build_execution_adapter,
    build_session_blocks,
    build_task_tree,
)
from conftest import THREE_BACKEND_OPTIONS as VERIFY_BACKEND_OPTIONS
from fastapi import HTTPException

from src.features.improve import api as improve_api
from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.models import DelegateRequest, SessionMetadata, TaskSpec, TaskType
from src.runtime import spawner_backends
from src.runtime.api import internal
from src.runtime.takeoff_gate import DelegationBlockedError
from src.runtime.task_execution import TaskExecutionAdapter
from src.runtime.task_sessions import TaskTreeManager


def _stub_task_manager():
  """A task-tree manager with an empty session lookup for tests that stub delegation."""
  from src.runtime.task_sessions import TaskTreeManager
  seam = _NoSessionSeam()
  return TaskTreeManager(
      CharlieBotConfig(charliebot_home=Path("/tmp/delegate-takeoff-stub")), seam, seam, seam, seam, seam, seam, seam,
      seam)


class _NoSessionSeam:
  """The no-op session seam the stub task-tree owner requires; it stands in for every block the tree takes."""

  async def get_session(self, session_id: str):
    return None

  def fresh_cached_metas(self):
    return {}


def _build_request(
    task_type: TaskType = TaskType.IMPLEMENT,
    repo_path: str | None = "/tmp/repo",
    base_branch: str | None = "main",
    backend: str | None = "codex-o3",
    session_id: str = "session-id",
) -> DelegateRequest:
  return DelegateRequest(
      session_id=session_id,
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
  session_blocks = AsyncMock()
  session_blocks.store.get_session.return_value = SessionMetadata(profile="manager", id=req.session_id, name="Test")
  task_mgr = _stub_task_manager()
  monkeypatch.setattr(task_mgr, "check_task_authorization", AsyncMock(side_effect=DelegationBlockedError("blocked")))

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(
        req, session_events=session_blocks.events, store=session_blocks.store, task_mgr=task_mgr)

  assert exc_info.value.status_code == 403
  assert exc_info.value.detail == "blocked"
  session_blocks.events.persist_and_broadcast.assert_not_awaited()


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
  session_blocks = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_events=session_blocks.events, store=session_blocks.store)

  assert exc_info.value.status_code == 400
  assert exc_info.value.detail == "verify delegations are repo-less; omit repo_path"
  session_blocks.store.get_session.assert_not_awaited()


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
@pytest.mark.parametrize("half", ["repo", "branch"])
async def test_delegate_task_repo_scoped_types_take_repo_and_base_together(task_type: TaskType, half: str) -> None:
  """Exactly one of repo_path/base_branch is a malformed repo-less delegation."""
  repo_path = "/tmp/repo" if half == "repo" else None
  base_branch = "main" if half == "branch" else None
  req = _build_request(task_type=task_type, repo_path=repo_path, base_branch=base_branch)
  session_blocks = AsyncMock()

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(req, session_events=session_blocks.events, store=session_blocks.store)

  assert exc_info.value.status_code == 400
  assert "together" in exc_info.value.detail
  session_blocks.store.get_session.assert_not_awaited()


@pytest.mark.asyncio
@pytest.mark.parametrize("task_type", [TaskType.IMPLEMENT, TaskType.QUICK_EDIT, TaskType.SCRIPT_RUN])
async def test_delegate_task_repo_less_request_passes_the_schema_gate(
    task_type: TaskType, monkeypatch: pytest.MonkeyPatch) -> None:
  """Omitting repo_path and base_branch together is a valid repo-less delegation."""
  req = _build_request(task_type=task_type, repo_path=None, base_branch=None)
  session_blocks = AsyncMock()
  session_blocks.store.get_session.return_value = SessionMetadata(profile="manager", id=req.session_id, name="Test")
  _patch_resolve_rig(monkeypatch)
  task_mgr = _stub_task_manager()
  task_mgr.check_task_authorization = AsyncMock()
  captured: dict = {}

  async def fake_delegate_task_tree(req, task_mgr, session_blocks, caller, backend, model):
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
      req, session_events=session_blocks.events, store=session_blocks.store, task_mgr=task_mgr, caller=None)

  assert result["session_id"] == "child"
  assert captured["repo_path"] is None
  assert captured["base_branch"] is None


@pytest.mark.asyncio
async def test_delegate_task_returns_400_for_invalid_backend(monkeypatch: pytest.MonkeyPatch) -> None:
  req = _build_request()
  session_blocks = AsyncMock()
  session_blocks.store.get_session.return_value = SessionMetadata(profile="manager", id=req.session_id, name="Test")
  task_mgr = _stub_task_manager()
  task_mgr.check_task_authorization = AsyncMock()

  async def fake_resolve_requested_subagent_backend_model(*args: Any, **kwargs: Any) -> tuple[str, str]:
    raise ValueError("requested backend 'codex-o3' is not in backends.options")

  monkeypatch.setattr(
      spawner_backends, "resolve_requested_subagent_backend_model", fake_resolve_requested_subagent_backend_model)
  monkeypatch.setattr(internal, "get_config", lambda: object())

  with pytest.raises(HTTPException) as exc_info:
    await internal.delegate_task(
        req, session_events=session_blocks.events, store=session_blocks.store, task_mgr=task_mgr)

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


# --- the worker child record carries the backend its creator resolved ---


def _backend_tree_env(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    preference: list[str],
) -> tuple[CharlieBotConfig, SessionBlocks, TaskTreeManager, TaskExecutionAdapter]:
  """A real task tree over a synthetic home, parent backend Y = OPUS_BACKEND_ID, launches disarmed.

  The tests below judge which backend the child record keeps and the
  dispatcher reserves; no Run ever executes, so the executor's scheduling
  seam is a no-op.
  """
  cfg = CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
      backends={
          "options": VERIFY_BACKEND_OPTIONS,
          "preference": preference
      },
  )
  session_blocks = build_session_blocks(cfg)
  tree = build_task_tree(cfg, session_blocks)
  adapter = build_execution_adapter(cfg, session_blocks, tree)
  monkeypatch.setattr(adapter, "_schedule_launch", lambda *args, **kwargs: None)
  tree.dispatch.executor = adapter
  tree.check_task_authorization = AsyncMock()
  monkeypatch.setattr(internal, "get_config", lambda: cfg)
  return cfg, session_blocks, tree, adapter


async def _backend_test_manager(tree: TaskTreeManager) -> SessionMetadata:
  """The delegating manager, pinned to the parent backend of the backend tests."""
  return await tree.create_task(
      request_id="root",
      task_parent_id=None,
      profile="manager",
      task=TaskSpec(goal="pm"),
      name="PM",
      backend=OPUS_BACKEND_ID,
      caller=OPERATOR,
  )


@pytest.mark.asyncio
async def test_delegate_child_and_its_later_runs_keep_the_requested_backend(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """--backend X pins the child record and the dispatcher's next work Run of it.

  The first work Run already records X; the child record must keep X too, so
  the run the dispatcher reserves on the next message stays on X.
  """
  _cfg, session_blocks, tree, adapter = _backend_tree_env(tmp_path, monkeypatch, preference=[OPUS_BACKEND_ID])
  manager = await _backend_test_manager(tree)

  result = await internal.delegate_task(
      _build_request(session_id=manager.id, backend="codex-o3"),
      session_events=session_blocks.events,
      store=session_blocks.store,
      task_mgr=tree,
      caller=OPERATOR,
  )

  child_id = result["session_id"]
  child = await tree.load_meta(child_id)
  assert child is not None and child.backend == "codex-o3"
  first = await tree.runs.get_run(child_id, result["run_id"])
  assert first is not None and first.backend == "codex-o3"

  # The first work Run settled; the parent's message reserves the next one.
  await tree.runs.record_finish(child_id, first.id, "success")
  await tree.dispatch.admit_input(
      child_id, event_type=ET.AGENT_MESSAGE, content="one more tweak", actor="agent", from_session=manager.id)
  decision = await tree.dispatch.dispatch_pending(child_id)

  assert decision["launch"] is True
  follow_up = await tree.runs.get_run(child_id, decision["run_id"])
  assert follow_up is not None and follow_up.id != first.id
  assert follow_up.kind == "work" and follow_up.backend == "codex-o3"

  # The review Run keeps its own rule: a backends.preference entry that
  # differs from the reviewed work Run's backend.
  await tree.runs.record_finish(child_id, follow_up.id, "success")
  review_id = await adapter._maybe_spawn_review(child_id, follow_up)
  review_run = await tree.runs.get_run(child_id, review_id)
  assert review_run is not None and review_run.kind == "review"
  assert review_run.backend != follow_up.backend
  assert review_run.backend == OPUS_BACKEND_ID


@pytest.mark.asyncio
async def test_verify_delegate_child_records_the_cross_model_backend(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A verify delegation without --backend: the child record keeps the
  cross-model backend its first work Run got from backends.preference."""
  _cfg, session_blocks, tree, _adapter = _backend_tree_env(
      tmp_path, monkeypatch, preference=[OPUS_BACKEND_ID, "codex-o3"])
  manager = await _backend_test_manager(tree)

  result = await internal.delegate_task(
      _build_request(session_id=manager.id, task_type=TaskType.VERIFY, repo_path=None, base_branch=None, backend=None),
      session_events=session_blocks.events,
      store=session_blocks.store,
      task_mgr=tree,
      caller=OPERATOR,
  )

  child = await tree.load_meta(result["session_id"])
  first = await tree.runs.get_run(result["session_id"], result["run_id"])
  assert first is not None and first.backend == "codex-o3"
  assert child is not None and child.backend == first.backend


@pytest.mark.asyncio
async def test_implement_delegate_without_backend_resolves_to_the_parent_backend(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An implement delegation without --backend resolves to the parent
  backend, and the child record keeps that value."""
  _cfg, session_blocks, tree, _adapter = _backend_tree_env(tmp_path, monkeypatch, preference=[OPUS_BACKEND_ID])
  manager = await _backend_test_manager(tree)

  result = await internal.delegate_task(
      _build_request(session_id=manager.id, backend=None),
      session_events=session_blocks.events,
      store=session_blocks.store,
      task_mgr=tree,
      caller=OPERATOR,
  )

  child = await tree.load_meta(result["session_id"])
  first = await tree.runs.get_run(result["session_id"], result["run_id"])
  assert first is not None and first.backend == OPUS_BACKEND_ID
  assert child is not None and child.backend == OPUS_BACKEND_ID


@pytest.mark.asyncio
async def test_improve_child_records_the_requested_backend(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An improve request naming backend X: the loop's worker child record keeps X."""
  cfg, session_blocks, tree, _adapter = _backend_tree_env(tmp_path, monkeypatch, preference=[OPUS_BACKEND_ID])
  manager = await _backend_test_manager(tree)
  spawned: list[str] = []

  def fake_logged_task(coro, *, name: str):
    # The loop controller is not this test's subject; the request is accepted
    # once the child exists, so the scheduled coroutine never starts here.
    coro.close()
    spawned.append(name)

  monkeypatch.setattr(improve_api, "create_logged_task", fake_logged_task)

  result = await improve_api.start_improve_loop(
      improve_api.ImproveRequest(
          session_id=manager.id,
          repo_path="/tmp/repo",
          base_branch="main",
          backend="codex-o3",
          goal="Improve this",
      ),
      cfg=cfg,
      store=session_blocks.store,
      task_mgr=tree,
  )

  child = await tree.load_meta(result["child_session_id"])
  assert child is not None and child.backend == "codex-o3"
  assert spawned, "the loop controller was never scheduled on an accepted request"
