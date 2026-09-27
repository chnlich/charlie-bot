"""Tests for stage D: fork copies plans.json + referenced artifacts, and the
sidebar pending-approval flag is computed server-side from the registry."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from conftest import OPUS_BACKEND_ID, make_home_session, plan_doc, user_event
from conftest import append_events as _append_events
from conftest import write_plans as _write_plans
from structlog.testing import capture_logs

from src.core.config import CharlieBotConfig
from src.core.models import CreateSessionRequest

_PLAN_V1_REL = "artifacts/plan_01.html"
_PLAN_V2_REL = "artifacts/plan_02.html"


def _make_version(v: int, file: str, verify_state: str) -> dict:
  return {
      "v": v,
      "file": file,
      "created_at": "2026-07-20T00:00:00+00:00",
      "trigger": "initial" if v == 1 else "feedback",
      "verify_thread": "th_" + str(v),
      "verify_state": verify_state,
      "base": None,
  }


def _write_artifact(cfg: CharlieBotConfig, session_id: str, file: str, content: str) -> Path:
  path = cfg.sessions_dir / session_id / file
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(content, encoding="utf-8")
  return path


# ---------------------------------------------------------------------------
# D3: fork copies plans.json + referenced artifacts
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_fork_copies_plans_json_and_referenced_artifacts(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend=OPUS_BACKEND_ID)
  _append_events(mgr.get_chat_events_path(parent.id), [user_event("e0")])

  _write_artifact(cfg, parent.id, _PLAN_V1_REL, "<html>v1</html>")
  _write_artifact(cfg, parent.id, _PLAN_V2_REL, "<html>v2</html>")
  _write_plans(
      cfg, parent.id, {
          "plans":
              [
                  plan_doc(
                      1, [
                          _make_version(1, _PLAN_V1_REL, "clean"),
                          _make_version(2, _PLAN_V2_REL, "pending"),
                      ],
                      title="My Plan"),
              ]
      })

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
async def test_fork_missing_artifact_logs_warning_and_does_not_abort(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend=OPUS_BACKEND_ID)
  _append_events(mgr.get_chat_events_path(parent.id), [user_event("e0")])

  _write_artifact(cfg, parent.id, _PLAN_V1_REL, "<html>present</html>")
  # plan_02.html is referenced but intentionally NOT created on disk.
  _write_plans(
      cfg, parent.id,
      {"plans": [plan_doc(1, [
          _make_version(1, _PLAN_V1_REL, "clean"),
          _make_version(2, _PLAN_V2_REL, "pending"),
      ]),]})

  with capture_logs() as logs:
    child = await mgr.fork_session(parent.id)

  # Fork succeeds; the existing artifact is copied; the missing one is skipped.
  assert (cfg.sessions_dir / child.id / "plans.json").exists()
  assert (cfg.sessions_dir / child.id / _PLAN_V1_REL).exists()
  assert not (cfg.sessions_dir / child.id / _PLAN_V2_REL).exists()
  child_plans = json.loads((cfg.sessions_dir / child.id / "plans.json").read_text(encoding="utf-8"))
  assert [ver["file"] for ver in child_plans["plans"][0]["versions"]] == [_PLAN_V1_REL, _PLAN_V2_REL]

  # A visible warning was logged for the missing file.
  assert any(
      entry.get("event") == "plan_artifact_missing_on_fork" and entry.get("log_level") == "warning"
      for entry in logs), f"expected plan_artifact_missing_on_fork warning, got: {logs}"


@pytest.mark.asyncio
async def test_fork_outside_parent_artifact_does_not_alias_copied_artifact(tmp_path: Path) -> None:
  cfg, mgr, parent = await make_home_session(tmp_path, name="Parent", backend=OPUS_BACKEND_ID)
  other = await mgr.create_session(CreateSessionRequest(name="Other"), backend=OPUS_BACKEND_ID)
  _append_events(mgr.get_chat_events_path(parent.id), [user_event("e0")])

  artifact_rel = "artifacts/collision.html"
  _write_artifact(cfg, parent.id, artifact_rel, "<html>parent</html>")
  external = _write_artifact(cfg, other.id, artifact_rel, "<html>external</html>")
  _write_plans(
      cfg, parent.id, {
          "plans":
              [
                  plan_doc(
                      1, [
                          _make_version(1, artifact_rel, "clean"),
                          _make_version(2, str(external.resolve()), "pending"),
                      ]),
              ]
      })

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


# ---------------------------------------------------------------------------
# D2: sidebar pending-approval flag (all_sessions_status)

# ---------------------------------------------------------------------------
# A1: corrupt registry — sidebar survives, plan_registry_read_failed warning

# ---------------------------------------------------------------------------
# A5: first-paint sidebar badge — GET /api/sessions/ carries has_pending_plan_approval
