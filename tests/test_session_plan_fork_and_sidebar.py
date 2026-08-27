"""Tests for stage D: fork copies plans.json + referenced artifacts, and the
sidebar pending-approval flag is computed server-side from the registry."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest
from conftest import append_events as _append_events
from conftest import make_home_session
from fastapi import FastAPI
from fastapi.testclient import TestClient
from structlog.testing import capture_logs

from src.api import sessions as sessions_api
from src.api.deps import get_session_manager
from src.api.sessions import router as sessions_router
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, SessionStatus
from src.core.sessions import SessionManager

_PLAN_V1_REL = "artifacts/plan_01.html"
_PLAN_V2_REL = "artifacts/plan_02.html"


def _make_plan(
    plan_id: int,
    versions: list[dict],
    *,
    title: str = "Plan",
    takeoff: dict | None = None,
    closed: dict | None = None,
) -> dict:
  return {
      "id": plan_id,
      "title": title,
      "versions": versions,
      "takeoff": takeoff,
      "closed": closed,
  }


def _make_version(v: int, file: str, verify_state: str = "pending") -> dict:
  return {
      "v": v,
      "file": file,
      "created_at": "2026-07-20T00:00:00+00:00",
      "trigger": "initial" if v == 1 else "feedback",
      "verify_thread": "th_" + str(v),
      "verify_state": verify_state,
      "base": None,
  }


def _write_plans(cfg: CharlieBotConfig, session_id: str, data: dict) -> Path:
  plans_path = cfg.sessions_dir / session_id / "plans.json"
  plans_path.parent.mkdir(parents=True, exist_ok=True)
  plans_path.write_text(json.dumps(data, indent=2), encoding="utf-8")
  return plans_path


def _write_artifact(cfg: CharlieBotConfig, session_id: str, file: str, content: str = "<html></html>") -> Path:
  path = cfg.sessions_dir / session_id / file
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(content, encoding="utf-8")
  return path


# ---------------------------------------------------------------------------
# D3: fork copies plans.json + referenced artifacts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_fork_copies_plans_json_and_referenced_artifacts(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  _write_artifact(cfg, parent.id, _PLAN_V1_REL, "<html>v1</html>")
  _write_artifact(cfg, parent.id, _PLAN_V2_REL, "<html>v2</html>")
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [
          _make_version(1, _PLAN_V1_REL, "clean"),
          _make_version(2, _PLAN_V2_REL, "pending"),
      ], title="My Plan"),
  ]})

  child = await mgr.fork_session(parent.id)

  child_plans_path = cfg.sessions_dir / child.id / "plans.json"
  assert child_plans_path.exists(), "child inherits plans.json"
  child_plans = json.loads(child_plans_path.read_text(encoding="utf-8"))
  parent_plans = json.loads((cfg.sessions_dir / parent.id / "plans.json").read_text(encoding="utf-8"))
  assert child_plans == parent_plans, "relative registry paths and all other fields carry over"

  assert (cfg.sessions_dir / child.id / _PLAN_V1_REL).exists()
  assert (cfg.sessions_dir / child.id / _PLAN_V2_REL).exists()
  assert (cfg.sessions_dir / child.id / _PLAN_V1_REL).read_text(encoding="utf-8") == "<html>v1</html>"


@pytest.mark.asyncio
async def test_fork_normalizes_absolute_in_session_paths_and_copies_distinct_files(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  artifact_rel = _PLAN_V1_REL
  artifact = _write_artifact(cfg, parent.id, artifact_rel, "<html>absolute</html>")
  parent_plans = _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [_make_version(1, str(artifact.resolve()), "clean")], title="Absolute"),
  ]})
  parent_plans_before = parent_plans.read_text(encoding="utf-8")
  artifact_before = artifact.read_text(encoding="utf-8")

  child = await mgr.fork_session(parent.id)

  child_plans_path = cfg.sessions_dir / child.id / "plans.json"
  child_plans = json.loads(child_plans_path.read_text(encoding="utf-8"))
  assert child_plans["plans"][0]["versions"][0]["file"] == artifact_rel
  assert str(cfg.sessions_dir / parent.id) not in child_plans_path.read_text(encoding="utf-8")
  child_artifact = cfg.sessions_dir / child.id / artifact_rel
  assert child_artifact.exists()
  assert child_artifact.read_text(encoding="utf-8") == artifact_before
  assert parent_plans.read_text(encoding="utf-8") == parent_plans_before
  assert artifact.read_text(encoding="utf-8") == artifact_before


@pytest.mark.asyncio
async def test_fork_normalizes_mixed_absolute_and_relative_paths(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  first_rel = _PLAN_V1_REL
  second_rel = _PLAN_V2_REL
  first = _write_artifact(cfg, parent.id, first_rel, "<html>v1</html>")
  _write_artifact(cfg, parent.id, second_rel, "<html>v2</html>")
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [
          _make_version(1, str(first.resolve()), "clean"),
          _make_version(2, second_rel, "pending"),
      ]),
  ]})

  child = await mgr.fork_session(parent.id)

  child_plans = json.loads((cfg.sessions_dir / child.id / "plans.json").read_text(encoding="utf-8"))
  assert [ver["file"] for ver in child_plans["plans"][0]["versions"]] == [first_rel, second_rel]
  assert (cfg.sessions_dir / child.id / first_rel).read_text(encoding="utf-8") == "<html>v1</html>"
  assert (cfg.sessions_dir / child.id / second_rel).read_text(encoding="utf-8") == "<html>v2</html>"


@pytest.mark.asyncio
async def test_fork_missing_artifact_logs_warning_and_does_not_abort(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  _write_artifact(cfg, parent.id, _PLAN_V1_REL, "<html>present</html>")
  # plan_02.html is referenced but intentionally NOT created on disk.
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [
          _make_version(1, _PLAN_V1_REL, "clean"),
          _make_version(2, _PLAN_V2_REL, "pending"),
      ]),
  ]})

  with capture_logs() as logs:
    child = await mgr.fork_session(parent.id)

  # Fork succeeds; the existing artifact is copied; the missing one is skipped.
  assert (cfg.sessions_dir / child.id / "plans.json").exists()
  assert (cfg.sessions_dir / child.id / _PLAN_V1_REL).exists()
  assert not (cfg.sessions_dir / child.id / _PLAN_V2_REL).exists()
  child_plans = json.loads((cfg.sessions_dir / child.id / "plans.json").read_text(encoding="utf-8"))
  assert [ver["file"] for ver in child_plans["plans"][0]["versions"]] == [
      _PLAN_V1_REL, _PLAN_V2_REL
  ]

  # A visible warning was logged for the missing file.
  assert any(
      entry.get("event") == "plan_artifact_missing_on_fork" and entry.get("log_level") == "warning"
      for entry in logs), f"expected plan_artifact_missing_on_fork warning, got: {logs}"


@pytest.mark.asyncio
async def test_fork_outside_parent_artifact_logs_warning_and_keeps_version(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  other = await mgr.create_session(CreateSessionRequest(name="Other"), backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  external_rel = "artifacts/external.html"
  external = _write_artifact(cfg, other.id, external_rel, "<html>external</html>")
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [_make_version(1, str(external.resolve()), "clean")]),
  ]})

  with capture_logs() as logs:
    child = await mgr.fork_session(parent.id)

  child_plans_path = cfg.sessions_dir / child.id / "plans.json"
  child_plans = json.loads(child_plans_path.read_text(encoding="utf-8"))
  child_file = child_plans["plans"][0]["versions"][0]["file"]
  assert child_file == external_rel
  assert not (cfg.sessions_dir / child.id / child_file).exists()
  assert external.read_text(encoding="utf-8") == "<html>external</html>"
  assert any(
      entry.get("event") == "plan_artifact_outside_parent_on_fork" and entry.get("log_level") == "warning"
      for entry in logs), f"expected outside-parent warning, got: {logs}"


@pytest.mark.asyncio
async def test_fork_outside_parent_artifact_does_not_alias_copied_artifact(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  other = await mgr.create_session(CreateSessionRequest(name="Other"), backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  artifact_rel = "artifacts/collision.html"
  _write_artifact(cfg, parent.id, artifact_rel, "<html>parent</html>")
  external = _write_artifact(cfg, other.id, artifact_rel, "<html>external</html>")
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [
          _make_version(1, artifact_rel, "clean"),
          _make_version(2, str(external.resolve()), "pending"),
      ]),
  ]})

  with capture_logs() as logs:
    child = await mgr.fork_session(parent.id)

  child_dir = cfg.sessions_dir / child.id
  child_plans = json.loads((child_dir / "plans.json").read_text(encoding="utf-8"))
  copied_file, external_file = [ver["file"] for ver in child_plans["plans"][0]["versions"]]
  assert copied_file == artifact_rel
  assert (child_dir / copied_file).read_text(encoding="utf-8") == "<html>parent</html>"
  assert external_file == "artifacts/collision.html.outside-1"
  assert not Path(external_file).is_absolute()
  assert ".." not in Path(external_file).parts
  assert not (child_dir / external_file).exists()
  assert external.read_text(encoding="utf-8") == "<html>external</html>"
  assert any(
      entry.get("event") == "plan_artifact_outside_parent_on_fork" and entry.get("log_level") == "warning"
      for entry in logs), f"expected outside-parent warning, got: {logs}"


@pytest.mark.asyncio
async def test_fork_without_plans_json_copies_nothing(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  child = await mgr.fork_session(parent.id)

  assert not (cfg.sessions_dir / child.id / "plans.json").exists()


@pytest.mark.asyncio
async def test_elone_also_copies_plans_and_artifacts(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  _write_artifact(cfg, parent.id, _PLAN_V1_REL, "<html>v1</html>")
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")]),
  ]})

  child = await mgr.elone_session(parent.id, event_index=0)

  assert (cfg.sessions_dir / child.id / "plans.json").exists()
  assert (cfg.sessions_dir / child.id / _PLAN_V1_REL).exists()


# ---------------------------------------------------------------------------
# D2: sidebar pending-approval flag (all_sessions_status)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_awaiting_approval(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Awaiting")

  # not closed, no takeoff, latest verify_state clean -> derived "awaiting approval"
  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")]),
  ]})

  status = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is True


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_approved_is_unset(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Approved")

  # takeoff set + verify_state clean -> derived "approved"
  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")],
                 takeoff={"v": 1, "at": "2026-07-20T00:00:00+00:00"}),
  ]})

  status = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_closed_is_unset(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Closed")

  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")],
                 closed={"as": "superseded", "at": "2026-07-20T00:00:00+00:00"}),
  ]})

  status = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_no_plans_json_is_unset(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="NoPlans")

  status = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_mixed_lineages(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Mixed")

  # One approved lineage + one awaiting-approval lineage -> flag set (at least one awaiting).
  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")],
                 takeoff={"v": 1, "at": "2026-07-20T00:00:00+00:00"}),
      _make_plan(2, [_make_version(1, _PLAN_V2_REL, "clean")]),
  ]})

  status = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is True


@pytest.mark.asyncio
async def test_pending_plan_approval_not_persisted_to_metadata(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="Awaiting")

  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")]),
  ]})

  status = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is True

  raw_metadata = json.loads((cfg.sessions_dir / session.id / "metadata.json").read_text(encoding="utf-8"))
  assert "has_pending_plan_approval" not in raw_metadata, "flag is transient and must not be persisted"


@pytest.mark.asyncio
async def test_pending_plan_approval_archived_session_is_unset(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="ArchivedAwaiting")

  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")]),
  ]})

  meta = await mgr.get_session(session.id)
  assert meta is not None
  meta.status = SessionStatus.ARCHIVED
  await mgr.save_metadata(meta)

  status = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_pending_plan_approval_no_parse_cost_without_plans_json(tmp_path: Path) -> None:
  cfg, mgr, session = await make_home_session(tmp_path, name="ShortCircuit")

  # The existence check short-circuits sessions without plans.json: the sync
  # helper returns False without parsing, and the status payload reflects it.
  assert await asyncio.to_thread(mgr._has_pending_plan_approval, session.id) is False

  status = await sessions_api.all_sessions_status(ids=session.id, session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


# ---------------------------------------------------------------------------
# A1: corrupt registry — sidebar survives, plan_registry_read_failed warning
# ---------------------------------------------------------------------------


def _build_sessions_app(session_mgr: SessionManager) -> FastAPI:
  """Minimal FastAPI app wiring only the sessions router + session manager override."""
  app = FastAPI()
  app.include_router(sessions_router, prefix="/api/sessions")
  app.dependency_overrides[get_session_manager] = lambda: session_mgr
  return app


@pytest.mark.asyncio
async def test_status_endpoint_survives_corrupt_plans_json_other_sessions_unaffected(tmp_path: Path) -> None:
  """Acceptance #1: corrupt plans.json on one session must not 5xx the sidebar poll."""
  cfg, mgr, good = await make_home_session(tmp_path, name="Good")
  bad = await mgr.create_session(CreateSessionRequest(name="Bad"))

  # good has an awaiting-approval lineage; bad has a corrupt plans.json.
  _write_plans(cfg, good.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")]),
  ]})
  bad_plans = cfg.sessions_dir / bad.id / "plans.json"
  bad_plans.parent.mkdir(parents=True, exist_ok=True)
  bad_plans.write_text("{not valid json", encoding="utf-8")

  app = _build_sessions_app(mgr)
  with TestClient(app) as client:
    resp = client.get(f"/api/sessions/status?ids={good.id},{bad.id}")

  assert resp.status_code == 200
  body = resp.json()
  assert body[good.id]["has_pending_plan_approval"] is True
  assert body[bad.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_corrupt_plans_json_logs_plan_registry_read_failed_warning(tmp_path: Path) -> None:
  """Reviewer A1: the probe logs plan_registry_read_failed with session_id and error."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Bad")
  plans_path = cfg.sessions_dir / session.id / "plans.json"
  plans_path.parent.mkdir(parents=True, exist_ok=True)
  plans_path.write_text("{not valid json", encoding="utf-8")

  with capture_logs() as logs:
    await asyncio.to_thread(mgr._has_pending_plan_approval, session.id)

  matching = [
      entry for entry in logs
      if entry.get("event") == "plan_registry_read_failed" and entry.get("log_level") == "warning"
  ]
  assert len(matching) == 1
  assert matching[0].get("session_id") == session.id
  assert isinstance(matching[0].get("error"), str) and matching[0]["error"]


@pytest.mark.asyncio
async def test_probe_partial_degradation_still_reports_pending_approval_true(tmp_path: Path) -> None:
  """Acceptance #2: one valid awaiting plan + one bad plan → probe still True (valid plan counts)."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Mixed")
  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")]),
      {"id": 2, "title": "Bad", "versions": [], "takeoff": None, "closed": None},
  ]})

  assert await asyncio.to_thread(mgr._has_pending_plan_approval, session.id) is True


# ---------------------------------------------------------------------------
# A5: first-paint sidebar badge — GET /api/sessions/ carries has_pending_plan_approval
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_list_sessions_endpoint_includes_pending_plan_approval_true(tmp_path: Path) -> None:
  """Acceptance #7: GET /api/sessions/ items include has_pending_plan_approval: true."""
  cfg, mgr, session = await make_home_session(tmp_path, name="Awaiting")
  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, _PLAN_V1_REL, "clean")]),
  ]})

  app = _build_sessions_app(mgr)
  with TestClient(app) as client:
    resp = client.get("/api/sessions/")

  assert resp.status_code == 200
  body = resp.json()
  item = next(s for s in body if s["id"] == session.id)
  assert item["has_pending_plan_approval"] is True


@pytest.mark.asyncio
async def test_list_sessions_endpoint_pending_plan_approval_false_without_plans_json(tmp_path: Path) -> None:
  """Sanity: a session with no plans.json reports has_pending_plan_approval: false on GET /."""
  cfg, mgr, session = await make_home_session(tmp_path, name="NoPlans")

  app = _build_sessions_app(mgr)
  with TestClient(app) as client:
    resp = client.get("/api/sessions/")

  assert resp.status_code == 200
  body = resp.json()
  item = next(s for s in body if s["id"] == session.id)
  assert item["has_pending_plan_approval"] is False
