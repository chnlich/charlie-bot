"""Tests for the plan internal-API endpoints and the sessions plans read path."""

from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock

import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.api.deps import get_plan_manager, get_session_manager, get_thread_manager
from src.api.internal import router as internal_router
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.models import (
    BackendOption,
    CreateSessionRequest,
    SessionMetadata,
    TaskType,
)
from src.core.plans import PlanRegistryManager
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

BACKEND_OPTIONS = [
    BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
]


def _write_stub_chrome(tmp_path: Path, height: int) -> str:
  """Write a fake headless-chrome binary printing a wrapper-shaped DOM with the chosen measured height."""
  stub = tmp_path / f"stub-chrome-{height}.sh"
  stub.write_text(f"#!/bin/sh\necho 'probe output <pre id=\"page-height\">{height}</pre>'\n", encoding="utf-8")
  stub.chmod(0o755)
  return str(stub)


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=BACKEND_OPTIONS,
      headless_chrome_bin=_write_stub_chrome(tmp_path, 800),
  )


def _build_app(
    cfg: CharlieBotConfig, session_mgr: SessionManager, thread_mgr: ThreadManager,
    plan_mgr: PlanRegistryManager) -> FastAPI:
  app = FastAPI()
  app.include_router(internal_router, prefix="/api/internal")
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  app.dependency_overrides[get_thread_manager] = lambda: thread_mgr
  app.dependency_overrides[get_plan_manager] = lambda: plan_mgr
  return app


def _write_artifact(cfg: CharlieBotConfig, session_id: str, name: str = "plan_01.html") -> str:
  artifacts_dir = cfg.sessions_dir / session_id / "artifacts"
  artifacts_dir.mkdir(parents=True, exist_ok=True)
  (artifacts_dir / name).write_text(
      "<html><section><h2>1 Problem / Goal</h2><p>Ship the fix.</p></section></html>", encoding="utf-8")
  return f"artifacts/{name}"


async def _setup(
    tmp_path: Path,) -> tuple[CharlieBotConfig, SessionManager, ThreadManager, PlanRegistryManager, SessionMetadata]:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  plan_mgr = PlanRegistryManager(cfg, session_mgr)
  meta = await session_mgr.create_session(CreateSessionRequest(name="Test"), backend="claude-opus-4.6")
  return cfg, session_mgr, thread_mgr, plan_mgr, meta


# ---------------------------------------------------------------------------
# Happy paths
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plan_present_endpoint_happy_path(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/present", json={
            "session_id": meta.id,
            "file": f,
            "title": "P1",
        })
  assert resp.status_code == 200
  assert resp.json() == {"plan": 1, "v": 1, "state": "awaiting approval"}


@pytest.mark.asyncio
async def test_plan_amend_endpoint_happy_path(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/amend", json={
            "session_id": meta.id,
            "file": f2,
            "plan_id": 1,
        })
  assert resp.status_code == 200
  body = resp.json()
  assert body == {"plan": 1, "v": 2, "state": "awaiting approval"}


@pytest.mark.asyncio
async def test_plan_approve_endpoint_happy_path(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f, title="P1")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post("/api/internal/plan/approve", json={"session_id": meta.id})
  assert resp.status_code == 200
  assert resp.json() == {"plan": 1, "v": 1, "state": "approved"}


@pytest.mark.asyncio
async def test_plan_close_endpoint_happy_path(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f, title="P1")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/close", json={
            "session_id": meta.id,
            "plan_id": 1,
            "close_as": "superseded",
        })
  assert resp.status_code == 200
  assert resp.json() == {"plan": 1, "state": "superseded"}


@pytest.mark.asyncio
async def test_plan_close_completed_then_list_shows_completed(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f, title="P1")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/close", json={
            "session_id": meta.id,
            "plan_id": 1,
            "close_as": "completed",
        })
    assert resp.status_code == 200
    assert resp.json() == {"plan": 1, "state": "completed"}

    listing = client.get(f"/api/sessions/{meta.id}/plans")
    assert listing.status_code == 200
    assert listing.json()["plans"][0]["state"] == "completed"

    second = client.post(
        "/api/internal/plan/close", json={
            "session_id": meta.id,
            "plan_id": 1,
            "close_as": "completed",
        })
  assert second.status_code == 400
  assert "already closed" in second.json()["detail"]


@pytest.mark.asyncio
async def test_get_plans_endpoint_returns_registry(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f, title="P1")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.get(f"/api/sessions/{meta.id}/plans")
  assert resp.status_code == 200
  body = resp.json()
  assert len(body["plans"]) == 1
  assert body["plans"][0]["state"] == "awaiting approval"
  assert body["plans"][0]["id"] == 1


# ---------------------------------------------------------------------------
# Rejections (one per endpoint)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plan_present_rejects_unknown_session_404(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, _meta = await _setup(tmp_path)
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/present", json={
            "session_id": "nonexistent",
            "file": "f.html",
            "title": "P1",
        })
  assert resp.status_code == 404
  assert resp.json()["detail"] == "Session not found"


@pytest.mark.asyncio
async def test_plan_present_rejects_missing_file_400(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/present", json={
            "session_id": meta.id,
            "file": "artifacts/missing.html",
            "title": "P1",
        })
  assert resp.status_code == 400
  assert "not found inside the session directory" in resp.json()["detail"]


@pytest.mark.asyncio
async def test_plan_amend_rejects_closed_400(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/amend", json={
            "session_id": meta.id,
            "file": f2,
            "plan_id": 1,
        })
  assert resp.status_code == 400
  assert "is closed" in resp.json()["detail"]


@pytest.mark.asyncio
async def test_plan_close_rejects_already_closed_400(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f, title="P1")
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/close", json={
            "session_id": meta.id,
            "plan_id": 1,
            "close_as": "abandoned",
        })
  assert resp.status_code == 400
  assert "already closed" in resp.json()["detail"]


@pytest.mark.asyncio
async def test_plan_reverify_endpoint_removed(tmp_path: Path) -> None:
  """The /plan/reverify endpoint is gone; FastAPI returns 404 (or 405) for the old path."""
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/reverify", json={
            "session_id": meta.id,
            "verify_thread": "t1",
            "plan_id": 1,
        })
  assert resp.status_code in (404, 405)


# ---------------------------------------------------------------------------
# Session view payload no longer carries plans (A6 — /view is plans-less)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_session_view_omits_plans(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f, title="P1")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.get(f"/api/sessions/{meta.id}/view")
  assert resp.status_code == 200
  body = resp.json()
  assert "plans" not in body


# ---------------------------------------------------------------------------
# plan_updated broadcast: emitted on mutation, absent from chat_events.jsonl
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plan_updated_broadcast_on_present_and_absent_from_chat_events(tmp_path: Path) -> None:
  cfg, session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  broadcast_calls: list[tuple[str, dict]] = []

  async def _capture_broadcast(session_id: str, event: dict) -> None:
    broadcast_calls.append((session_id, event))

  session_mgr.broadcast_only = _capture_broadcast  # type: ignore[method-assign]
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  app = _build_app(cfg, session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/present", json={
            "session_id": meta.id,
            "file": f,
            "title": "P1",
        })
  assert resp.status_code == 200

  plan_updates = [e for _sid, e in broadcast_calls if e.get("type") == "plan_updated"]
  assert len(plan_updates) == 1
  assert plan_updates[0] == {"type": "plan_updated", "session_id": meta.id, "plan_id": 1}

  events_path = session_mgr.get_chat_events_path(meta.id)
  if events_path.exists():
    raw = events_path.read_text(encoding="utf-8")
    assert "plan_updated" not in raw


# ---------------------------------------------------------------------------
# ThreadMetadata.task_type is set on delegate-created threads
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_delegate_sets_task_type_on_thread(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  from src.api import internal
  from src.core.models import DelegateRequest, SpawnRequest, TaskType

  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  meta = await session_mgr.create_session(CreateSessionRequest(name="Test"), backend="claude-opus-4.6")
  captured_thread: dict[str, Any] = {}

  async def fake_spawn_worker(*args: Any, **kwargs: Any) -> None:
    return None

  def fake_create_logged_task(coro: Any, *, name: str | None = None) -> Any:
    del name
    if coro.cr_frame is not None:
      captured_thread.update(coro.cr_frame.f_locals)
    coro.close()
    return object()

  async def fake_resolve(*args: Any, **kwargs: Any) -> tuple[str, str]:
    return "claude-opus-4.6", "claude-opus-4-6"

  monkeypatch.setattr(internal, "resolve_requested_subagent_backend_model", fake_resolve)
  monkeypatch.setattr(internal, "spawn_worker", fake_spawn_worker)
  monkeypatch.setattr(internal, "create_logged_task", fake_create_logged_task)
  monkeypatch.setattr(internal, "get_config", lambda: cfg)
  monkeypatch.setattr(internal, "check_takeoff_gate", lambda *a, **k: None)

  req = DelegateRequest(
      session_id=meta.id,
      description="verify this plan",
      task_type=TaskType.VERIFY,
  )
  result = await internal.delegate_task(req, session_mgr=session_mgr, thread_mgr=thread_mgr)
  assert result["thread_id"]

  thread = await thread_mgr.get_thread(meta.id, result["thread_id"])
  assert thread is not None
  assert thread.task_type == TaskType.VERIFY


# ---------------------------------------------------------------------------
# List endpoint contract (A2) — 404 unknown / 200+errors corrupt / 200+empty errors normal
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_get_plans_endpoint_unknown_session_404(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, _meta = await _setup(tmp_path)
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.get("/api/sessions/nonexistent/plans")
  assert resp.status_code == 404
  assert resp.json()["detail"] == "Session not found"


@pytest.mark.asyncio
async def test_get_plans_endpoint_corrupt_file_200_with_error_entry(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  plans_path = cfg.sessions_dir / meta.id / "plans.json"
  plans_path.parent.mkdir(parents=True, exist_ok=True)
  plans_path.write_text("{not valid json", encoding="utf-8")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.get(f"/api/sessions/{meta.id}/plans")
  assert resp.status_code == 200
  body = resp.json()
  assert body["plans"] == []
  assert len(body["errors"]) == 1
  assert body["errors"][0]["plan_id"] is None
  assert body["errors"][0]["session_id"] == meta.id


@pytest.mark.asyncio
async def test_get_plans_endpoint_normal_file_200_with_empty_errors(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f, title="P1")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.get(f"/api/sessions/{meta.id}/plans")
  assert resp.status_code == 200
  body = resp.json()
  assert len(body["plans"]) == 1
  assert body["errors"] == []


# ---------------------------------------------------------------------------
# Amend trigger tightening (A4) — API request with trigger=initial → 422 (pydantic)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plan_amend_rejects_initial_trigger_422(tmp_path: Path) -> None:
  cfg, _session_mgr, thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  app = _build_app(cfg, _session_mgr, thread_mgr, plan_mgr)
  with TestClient(app) as client:
    resp = client.post(
        "/api/internal/plan/amend", json={
            "session_id": meta.id,
            "file": f2,
            "plan_id": 1,
            "trigger": "initial",
        })
  assert resp.status_code == 422
