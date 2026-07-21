"""Tests for stage D: fork copies plans.json + referenced artifacts, and the
sidebar pending-approval flag is computed server-side from the registry."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

import pytest
from structlog.testing import capture_logs

from src.api import sessions as sessions_api
from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest, SessionStatus
from src.core.sessions import SessionManager


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


def _append_events(path: Path, events: list[dict]) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  with open(path, "a", encoding="utf-8") as f:
    for event in events:
      f.write(json.dumps(event) + "\n")


# ---------------------------------------------------------------------------
# D3: fork copies plans.json + referenced artifacts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_fork_copies_plans_json_and_referenced_artifacts(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  _write_artifact(cfg, parent.id, "artifacts/plan_01.html", "<html>v1</html>")
  _write_artifact(cfg, parent.id, "artifacts/plan_02.html", "<html>v2</html>")
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [
          _make_version(1, "artifacts/plan_01.html", "clean"),
          _make_version(2, "artifacts/plan_02.html", "pending"),
      ], title="My Plan"),
  ]})

  child = await mgr.fork_session(parent.id)

  child_plans_path = cfg.sessions_dir / child.id / "plans.json"
  assert child_plans_path.exists(), "child inherits plans.json"
  child_plans = json.loads(child_plans_path.read_text(encoding="utf-8"))
  parent_plans = json.loads((cfg.sessions_dir / parent.id / "plans.json").read_text(encoding="utf-8"))
  assert child_plans == parent_plans, "registry content is copied unmodified (ids, versions, states carry over)"

  assert (cfg.sessions_dir / child.id / "artifacts/plan_01.html").exists()
  assert (cfg.sessions_dir / child.id / "artifacts/plan_02.html").exists()
  assert (cfg.sessions_dir / child.id / "artifacts/plan_01.html").read_text(encoding="utf-8") == "<html>v1</html>"


@pytest.mark.asyncio
async def test_fork_missing_artifact_logs_warning_and_does_not_abort(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  _write_artifact(cfg, parent.id, "artifacts/plan_01.html", "<html>present</html>")
  # plan_02.html is referenced but intentionally NOT created on disk.
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [
          _make_version(1, "artifacts/plan_01.html", "clean"),
          _make_version(2, "artifacts/plan_02.html", "pending"),
      ]),
  ]})

  with capture_logs() as logs:
    child = await mgr.fork_session(parent.id)

  # Fork succeeds; the existing artifact is copied; the missing one is skipped.
  assert (cfg.sessions_dir / child.id / "plans.json").exists()
  assert (cfg.sessions_dir / child.id / "artifacts/plan_01.html").exists()
  assert not (cfg.sessions_dir / child.id / "artifacts/plan_02.html").exists()

  # A visible warning was logged for the missing file.
  assert any(
      entry.get("event") == "plan_artifact_missing_on_fork" and entry.get("log_level") == "warning"
      for entry in logs), f"expected plan_artifact_missing_on_fork warning, got: {logs}"


@pytest.mark.asyncio
async def test_fork_without_plans_json_copies_nothing(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  child = await mgr.fork_session(parent.id)

  assert not (cfg.sessions_dir / child.id / "plans.json").exists()


@pytest.mark.asyncio
async def test_elone_also_copies_plans_and_artifacts(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  parent = await mgr.create_session(CreateSessionRequest(name="Parent"), backend="claude-opus-4.6")
  _append_events(mgr.get_chat_events_path(parent.id), [{"type": "user", "content": "e0"}])

  _write_artifact(cfg, parent.id, "artifacts/plan_01.html", "<html>v1</html>")
  _write_plans(cfg, parent.id, {"plans": [
      _make_plan(1, [_make_version(1, "artifacts/plan_01.html", "clean")]),
  ]})

  child = await mgr.elone_session(parent.id, event_index=0)

  assert (cfg.sessions_dir / child.id / "plans.json").exists()
  assert (cfg.sessions_dir / child.id / "artifacts/plan_01.html").exists()


# ---------------------------------------------------------------------------
# D2: sidebar pending-approval flag (all_sessions_status)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_awaiting_approval(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Awaiting"))

  # not closed, no takeoff, latest verify_state clean -> derived "awaiting approval"
  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, "artifacts/plan_01.html", "clean")]),
  ]})

  status = await sessions_api.all_sessions_status(session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is True


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_approved_is_unset(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Approved"))

  # takeoff set + verify_state clean -> derived "approved"
  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, "artifacts/plan_01.html", "clean")],
                 takeoff={"v": 1, "at": "2026-07-20T00:00:00+00:00"}),
  ]})

  status = await sessions_api.all_sessions_status(session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_closed_is_unset(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Closed"))

  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, "artifacts/plan_01.html", "clean")],
                 closed={"as": "superseded", "at": "2026-07-20T00:00:00+00:00"}),
  ]})

  status = await sessions_api.all_sessions_status(session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_no_plans_json_is_unset(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="NoPlans"))

  status = await sessions_api.all_sessions_status(session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_all_sessions_status_pending_plan_approval_mixed_lineages(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Mixed"))

  # One approved lineage + one awaiting-approval lineage -> flag set (at least one awaiting).
  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, "artifacts/plan_01.html", "clean")],
                 takeoff={"v": 1, "at": "2026-07-20T00:00:00+00:00"}),
      _make_plan(2, [_make_version(1, "artifacts/plan_02.html", "clean")]),
  ]})

  status = await sessions_api.all_sessions_status(session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is True


@pytest.mark.asyncio
async def test_pending_plan_approval_not_persisted_to_metadata(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="Awaiting"))

  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, "artifacts/plan_01.html", "clean")]),
  ]})

  status = await sessions_api.all_sessions_status(session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is True

  raw_metadata = json.loads((cfg.sessions_dir / session.id / "metadata.json").read_text(encoding="utf-8"))
  assert "has_pending_plan_approval" not in raw_metadata, "flag is transient and must not be persisted"


@pytest.mark.asyncio
async def test_pending_plan_approval_archived_session_is_unset(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="ArchivedAwaiting"))

  _write_plans(cfg, session.id, {"plans": [
      _make_plan(1, [_make_version(1, "artifacts/plan_01.html", "clean")]),
  ]})

  meta = await mgr.get_session(session.id)
  assert meta is not None
  meta.status = SessionStatus.ARCHIVED
  await mgr._save_metadata(meta)

  status = await sessions_api.all_sessions_status(session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False


@pytest.mark.asyncio
async def test_pending_plan_approval_no_parse_cost_without_plans_json(tmp_path: Path) -> None:
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home")
  mgr = SessionManager(cfg)
  session = await mgr.create_session(CreateSessionRequest(name="ShortCircuit"))

  # The existence check short-circuits sessions without plans.json: the sync
  # helper returns False without parsing, and the status payload reflects it.
  assert await asyncio.to_thread(mgr._has_pending_plan_approval, session.id) is False

  status = await sessions_api.all_sessions_status(session_mgr=mgr)
  assert status[session.id]["has_pending_plan_approval"] is False
