"""Tests for the plan registry: state machine, rejections, derived state, schema migration."""

import json
from pathlib import Path

import pytest
from conftest import plan_page_html
from conftest import make_plan_setup as _setup
from conftest import write_plan_artifact as _write_artifact

from src.core.config import CharlieBotConfig
from src.core.plans import (
    PlanRegistryManager,
    read_plans_tolerant,
)


async def _present_first_plan(plan_mgr: PlanRegistryManager, cfg: CharlieBotConfig, meta_id: str) -> str:
  """Write the default plan artifact and register it as plan 1 v1 (title P1); returns the
  artifact's plan-relative path for the tests that re-present or rebind that file."""
  file_rel = _write_artifact(cfg, meta_id, "plan_01.html")
  await plan_mgr.present(meta_id, file=file_rel, title="P1")
  return file_rel


# ---------------------------------------------------------------------------
# Derived-state truth table (pure function of closed, takeoff)


# ---------------------------------------------------------------------------
# State machine: present → approve → amend → close
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_present_returns_awaiting_approval(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")

  result = await plan_mgr.present(meta.id, file=file_rel, title="P1")
  assert result == {"plan": 1, "v": 1, "state": "awaiting approval"}


@pytest.mark.asyncio
@pytest.mark.parametrize("first_absolute", [False, True])
@pytest.mark.parametrize("amend", [False, True], ids=["present", "amend"])
async def test_registry_rejects_cross_format_duplicate(tmp_path: Path, first_absolute: bool, amend: bool) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  file_abs = str((cfg.sessions_dir / meta.id / file_rel).resolve())
  first_file = file_abs if first_absolute else file_rel
  second_file = file_rel if first_absolute else file_abs

  await plan_mgr.present(meta.id, file=first_file, title="P1")
  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    if amend:
      await plan_mgr.amend(meta.id, file=second_file, plan_id=1, note="why changed")
    else:
      await plan_mgr.present(meta.id, file=second_file, title="P2")


@pytest.mark.asyncio
async def test_approve_returns_approved(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  await _present_first_plan(plan_mgr, cfg, meta.id)

  result = await plan_mgr.approve(meta.id)
  assert result == {"plan": 1, "v": 1, "state": "approved"}


# ---------------------------------------------------------------------------
# Version note: present records null, amend requires a non-empty one
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
@pytest.mark.parametrize("note", [None, "", "   "])
async def test_amend_requires_non_empty_note(tmp_path: Path, note: str | None) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  await _present_first_plan(plan_mgr, cfg, meta.id)
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")

  with pytest.raises(ValueError, match="amend requires a non-empty --note"):
    await plan_mgr.amend(meta.id, file=f2, plan_id=1, note=note)


@pytest.mark.asyncio
async def test_close_superseded_and_abandoned(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  await _present_first_plan(plan_mgr, cfg, meta.id)

  result = await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")
  assert result == {"plan": 1, "state": "superseded"}

  file_2 = _write_artifact(cfg, meta.id, "plan_02.html")
  await plan_mgr.present(meta.id, file=file_2, title="P2")
  result = await plan_mgr.close(meta.id, plan_id=2, close_as="abandoned")
  assert result == {"plan": 2, "state": "abandoned"}


@pytest.mark.asyncio
async def test_closing_already_closed_rejected(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  await _present_first_plan(plan_mgr, cfg, meta.id)
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")

  with pytest.raises(ValueError, match="already closed"):
    await plan_mgr.close(meta.id, plan_id=1, close_as="abandoned")


# ---------------------------------------------------------------------------
# Rejections


# ---------------------------------------------------------------------------
# Persistence, schema, and migration
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_plans_json_shape_matches_schema(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1", base={"repo": "r", "branch": "b", "sha": "s"})

  raw = (cfg.sessions_dir / meta.id / "plans.json").read_text(encoding="utf-8")
  data = json.loads(raw)
  assert list(data.keys()) == ["plans"]
  plan = data["plans"][0]
  assert set(plan.keys()) == {"id", "title", "versions", "takeoff", "closed"}
  ver = plan["versions"][0]
  assert set(ver.keys()) == {"v", "file", "created_at", "trigger", "base", "note"}
  assert ver["v"] == 1
  assert ver["file"] == "artifacts/plan_01.html"
  assert ver["trigger"] == "initial"
  assert ver["base"] == {"repo": "r", "branch": "b", "sha": "s"}
  assert ver["note"] is None
  assert plan["takeoff"] is None
  assert plan["closed"] is None


# ---------------------------------------------------------------------------
# Broadcast


# ---------------------------------------------------------------------------
# Enum reservations


# ---------------------------------------------------------------------------
# Tolerant read path (A1) — single authority in plans.py
# ---------------------------------------------------------------------------


def test_read_plans_tolerant_corrupt_json_returns_one_file_level_error(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  p.write_text("{not valid json", encoding="utf-8")
  result = read_plans_tolerant(p, "sess")
  assert result["plans"] == []
  assert len(result["errors"]) == 1
  err = result["errors"][0]
  assert err["session_id"] == "sess"
  assert err["plan_id"] is None
  assert isinstance(err["error"], str) and err["error"]


# ---------------------------------------------------------------------------
# Path normalization at the verb boundary (A3)


# ---------------------------------------------------------------------------
# Amend trigger tightening (A4) — initial writable only by present


# ---------------------------------------------------------------------------
# Goal budget gate: present/amend reject an over-budget Problem / Goal section
# ---------------------------------------------------------------------------


def _goal_doc(goal_text: str) -> str:
  """A page passing every plan assertion except goal-budget, carrying *goal_text* as the goal body."""
  return plan_page_html(goal_body=goal_text)


@pytest.mark.asyncio
async def test_present_rejects_goal_over_budget_with_measured_value(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html", content=_goal_doc("x" * 241))
  with pytest.raises(ValueError, match=r"241 weighted chars \(budget 240\)"):
    await plan_mgr.present(meta.id, file=file_rel, title="P1")


# ---------------------------------------------------------------------------
# Page budget gate: present/amend reject artifacts over the 2000 px height budget


# ---------------------------------------------------------------------------
# DOM assertions: present/amend enforce the full plan assertion set, not just budgets


# ---------------------------------------------------------------------------
# Fork-explainer gate: present/amend enforce the open Trade-off explainer


# ---------------------------------------------------------------------------
# Event-loop responsiveness: the assertion run (a headless-Chrome subprocess) is off-loop
