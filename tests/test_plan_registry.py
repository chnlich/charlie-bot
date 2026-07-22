"""Tests for the plan registry: state machine, rejections, derived state, schema migration."""

import json
from pathlib import Path

import pytest

from src.core.config import CharlieBotConfig
from src.core.models import (
    BackendOption,
    CreateSessionRequest,
    SessionMetadata,
)
from src.core.plans import (
    PlanRegistryManager,
    _DerivedState,
    derive_state_str,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

BACKEND_OPTIONS = [
    BackendOption(id="claude-opus-4.6", label="Opus", type="cc-claude", model="claude-opus-4-6"),
]


def _build_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "charliebot-home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=BACKEND_OPTIONS,
  )


def _write_artifact(cfg: CharlieBotConfig, session_id: str, name: str = "plan_01.html") -> str:
  artifacts_dir = cfg.sessions_dir / session_id / "artifacts"
  artifacts_dir.mkdir(parents=True, exist_ok=True)
  (artifacts_dir / name).write_text("<html>plan</html>", encoding="utf-8")
  return f"artifacts/{name}"


async def _setup(
    tmp_path: Path,) -> tuple[CharlieBotConfig, SessionManager, ThreadManager, PlanRegistryManager, SessionMetadata]:
  cfg = _build_cfg(tmp_path)
  session_mgr = SessionManager(cfg)
  thread_mgr = ThreadManager(cfg)
  plan_mgr = PlanRegistryManager(cfg, session_mgr, thread_mgr)
  meta = await session_mgr.create_session(CreateSessionRequest(name="Test"), backend="claude-opus-4.6")
  return cfg, session_mgr, thread_mgr, plan_mgr, meta


# ---------------------------------------------------------------------------
# Derived-state truth table (pure function of closed, takeoff)
# ---------------------------------------------------------------------------


def _plan(closed=None, takeoff=None) -> dict:
  return {
      "id": 1,
      "title": "P",
      "versions":
          [
              {
                  "v": 1,
                  "file": "artifacts/plan_01.html",
                  "created_at": "2026-07-20T00:00:00+00:00",
                  "trigger": "initial",
                  "base": None,
              }
          ],
      "takeoff": takeoff,
      "closed": closed,
  }


@pytest.mark.parametrize(
    ("closed", "takeoff", "expected"),
    [
        ({
            "as": "superseded",
            "at": "2026-07-20T00:00:00+00:00"
        }, None, "superseded"),
        (
            {
                "as": "superseded",
                "at": "2026-07-20T00:00:00+00:00"
            }, {
                "v": 1,
                "at": "2026-07-20T00:00:00+00:00"
            }, "superseded"),
        ({
            "as": "abandoned",
            "at": "2026-07-20T00:00:00+00:00"
        }, None, "abandoned"),
        (None, None, "awaiting approval"),
        (None, {
            "v": 1,
            "at": "2026-07-20T00:00:00+00:00"
        }, "approved"),
    ],
)
def test_derive_state_str_truth_table(closed, takeoff, expected) -> None:
  assert derive_state_str(_plan(closed=closed, takeoff=takeoff)) == expected


def test_derive_state_str_unknown_closed_as_raises() -> None:
  with pytest.raises(ValueError, match="unknown closed.as"):
    derive_state_str(_plan(closed={"as": "weird", "at": "x"}))


def test_derive_state_str_empty_versions_raises() -> None:
  with pytest.raises(ValueError, match="has no versions"):
    derive_state_str({"id": 1, "title": "P", "versions": [], "takeoff": None, "closed": None})


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
async def test_present_absolute_in_session_path_stores_relative_path(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  file_abs = str((cfg.sessions_dir / meta.id / file_rel).resolve())

  await plan_mgr.present(meta.id, file=file_abs, title="P1")

  data = json.loads((cfg.sessions_dir / meta.id / "plans.json").read_text(encoding="utf-8"))
  assert data["plans"][0]["versions"][0]["file"] == file_rel


@pytest.mark.asyncio
async def test_amend_absolute_in_session_path_stores_relative_path(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_1, title="P1")
  file_2 = _write_artifact(cfg, meta.id, "plan_02.html")
  file_2_abs = str((cfg.sessions_dir / meta.id / file_2).resolve())

  await plan_mgr.amend(meta.id, file=file_2_abs, plan_id=1)

  data = json.loads((cfg.sessions_dir / meta.id / "plans.json").read_text(encoding="utf-8"))
  assert [ver["file"] for ver in data["plans"][0]["versions"]] == [file_1, file_2]


@pytest.mark.asyncio
@pytest.mark.parametrize("first_absolute", [False, True])
async def test_present_rejects_cross_format_duplicate(tmp_path: Path, first_absolute: bool) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  file_abs = str((cfg.sessions_dir / meta.id / file_rel).resolve())
  first_file = file_abs if first_absolute else file_rel
  second_file = file_rel if first_absolute else file_abs

  await plan_mgr.present(meta.id, file=first_file, title="P1")
  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    await plan_mgr.present(meta.id, file=second_file, title="P2")


@pytest.mark.asyncio
@pytest.mark.parametrize("first_absolute", [False, True])
async def test_amend_rejects_cross_format_duplicate(tmp_path: Path, first_absolute: bool) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  file_abs = str((cfg.sessions_dir / meta.id / file_rel).resolve())
  first_file = file_abs if first_absolute else file_rel
  second_file = file_rel if first_absolute else file_abs

  await plan_mgr.present(meta.id, file=first_file, title="P1")
  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    await plan_mgr.amend(meta.id, file=second_file, plan_id=1)


@pytest.mark.asyncio
async def test_approve_returns_approved(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, title="P1")

  result = await plan_mgr.approve(meta.id)
  assert result == {"plan": 1, "v": 1, "state": "approved"}


@pytest.mark.asyncio
async def test_present_without_verify_thread_then_approve_succeeds(tmp_path: Path) -> None:
  """verify_thread is gone from present; approve is unconditional (no verify_state to read)."""
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")

  present_result = await plan_mgr.present(meta.id, file=file_rel, title="P1")
  assert present_result == {"plan": 1, "v": 1, "state": "awaiting approval"}

  approve_result = await plan_mgr.approve(meta.id)
  assert approve_result == {"plan": 1, "v": 1, "state": "approved"}


@pytest.mark.asyncio
async def test_approve_unconditional_no_verify_state_field(tmp_path: Path) -> None:
  """A version dict that would previously have been verify_state=mismatch approves unconditionally."""
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, title="P1")

  # Mutate the on-disk registry to inject a legacy verify_state=mismatch field on the version,
  # simulating an old-shape registry. approve must still succeed (no verify_state to read).
  plans_path = cfg.sessions_dir / meta.id / "plans.json"
  raw = plans_path.read_text(encoding="utf-8")
  data = json.loads(raw)
  data["plans"][0]["versions"][0]["verify_state"] = "mismatch"
  plans_path.write_text(json.dumps(data, indent=2), encoding="utf-8")

  result = await plan_mgr.approve(meta.id)
  assert result == {"plan": 1, "v": 1, "state": "approved"}


@pytest.mark.asyncio
async def test_amend_on_approved_clears_takeoff(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_1, title="P1")
  await plan_mgr.approve(meta.id)

  file_2 = _write_artifact(cfg, meta.id, "plan_02.html")
  result = await plan_mgr.amend(meta.id, file=file_2, plan_id=1)
  assert result == {"plan": 1, "v": 2, "state": "awaiting approval"}

  listing = await plan_mgr.list_plans(meta.id)
  assert listing["plans"][0]["takeoff"] is None
  assert len(listing["plans"][0]["versions"]) == 2


@pytest.mark.asyncio
async def test_close_superseded_and_abandoned(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, title="P1")

  result = await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")
  assert result == {"plan": 1, "state": "superseded"}

  file_2 = _write_artifact(cfg, meta.id, "plan_02.html")
  await plan_mgr.present(meta.id, file=file_2, title="P2")
  result = await plan_mgr.close(meta.id, plan_id=2, close_as="abandoned")
  assert result == {"plan": 2, "state": "abandoned"}


@pytest.mark.asyncio
async def test_closing_already_closed_rejected(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, title="P1")
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")

  with pytest.raises(ValueError, match="already closed"):
    await plan_mgr.close(meta.id, plan_id=1, close_as="abandoned")


# ---------------------------------------------------------------------------
# Rejections
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_present_rejects_missing_file(tmp_path: Path) -> None:
  _cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  with pytest.raises(ValueError, match="not found inside the session directory"):
    await plan_mgr.present(meta.id, file="artifacts/missing.html", title="P1")


@pytest.mark.asyncio
async def test_present_rejects_file_outside_session_dir(tmp_path: Path) -> None:
  _cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  with pytest.raises(ValueError, match="resolves outside the session directory"):
    await plan_mgr.present(meta.id, file="../escape.html", title="P1")


@pytest.mark.asyncio
async def test_present_rejects_already_bound_file(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=file_rel, title="P1")
  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    await plan_mgr.present(meta.id, file=file_rel, title="P2")


@pytest.mark.asyncio
async def test_approve_ambiguity_requires_plan(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  await plan_mgr.present(meta.id, file=f2, title="P2")

  with pytest.raises(ValueError, match="approve requires --plan"):
    await plan_mgr.approve(meta.id)


@pytest.mark.asyncio
async def test_amend_rejects_closed(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  with pytest.raises(ValueError, match="is closed"):
    await plan_mgr.amend(meta.id, file=f2, plan_id=1)


@pytest.mark.asyncio
async def test_amend_ambiguity_requires_plan(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  await plan_mgr.present(meta.id, file=f2, title="P2")

  f3 = _write_artifact(cfg, meta.id, "plan_03.html")
  with pytest.raises(ValueError, match="amend requires --plan"):
    await plan_mgr.amend(meta.id, file=f3)


@pytest.mark.asyncio
async def test_amend_no_open_lineage_requires_plan(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  await plan_mgr.close(meta.id, plan_id=1, close_as="superseded")

  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  with pytest.raises(ValueError, match="no open lineage to amend"):
    await plan_mgr.amend(meta.id, file=f2)


@pytest.mark.asyncio
async def test_close_rejects_invalid_close_as(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  with pytest.raises(ValueError, match="--as must be superseded|abandoned"):
    await plan_mgr.close(meta.id, plan_id=1, close_as="weird")


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
  assert set(ver.keys()) == {"v", "file", "created_at", "trigger", "base"}
  assert ver["v"] == 1
  assert ver["file"] == "artifacts/plan_01.html"
  assert ver["trigger"] == "initial"
  assert ver["base"] == {"repo": "r", "branch": "b", "sha": "s"}
  assert plan["takeoff"] is None
  assert plan["closed"] is None


@pytest.mark.asyncio
async def test_missing_plans_json_is_empty_registry(tmp_path: Path) -> None:
  _cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  listing = await plan_mgr.list_plans(meta.id)
  assert listing == {"plans": []}


@pytest.mark.asyncio
async def test_old_plans_json_with_verify_keys_loads_and_list_omits_them(tmp_path: Path) -> None:
  """Migration: an old plans.json with verify_thread/verify_state keys loads fine, and list_plans
  output (and any later save) drops those legacy keys since the registry projects to the current schema."""
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  plans_path = cfg.sessions_dir / meta.id / "plans.json"
  plans_path.parent.mkdir(parents=True, exist_ok=True)
  old_shape = {
      "plans":
          [
              {
                  "id": 1,
                  "title": "Legacy",
                  "versions":
                      [
                          {
                              "v": 1,
                              "file": "artifacts/plan_01.html",
                              "created_at": "2026-07-20T00:00:00+00:00",
                              "trigger": "initial",
                              "verify_thread": "t1",
                              "verify_state": "clean",
                              "base": None,
                          }
                      ],
                  "takeoff": None,
                  "closed": None,
              }
          ]
  }
  plans_path.write_text(json.dumps(old_shape, indent=2), encoding="utf-8")

  listing = await plan_mgr.list_plans(meta.id)
  assert len(listing["plans"]) == 1
  plan = listing["plans"][0]
  assert set(plan.keys()) == {"id", "title", "versions", "takeoff", "closed", "state"}
  ver = plan["versions"][0]
  assert set(ver.keys()) == {"v", "file", "created_at", "trigger", "base"}
  assert "verify_thread" not in ver
  assert "verify_state" not in ver
  # The legacy verify_state=clean collapses to awaiting approval (no takeoff, no closed).
  assert plan["state"] == "awaiting approval"


@pytest.mark.asyncio
async def test_save_drops_legacy_verify_keys(tmp_path: Path) -> None:
  """A mutation-triggered save rewrites the file with only the current schema fields."""
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  plans_path = cfg.sessions_dir / meta.id / "plans.json"
  plans_path.parent.mkdir(parents=True, exist_ok=True)
  old_shape = {
      "plans":
          [
              {
                  "id": 1,
                  "title": "Legacy",
                  "versions":
                      [
                          {
                              "v": 1,
                              "file": "artifacts/plan_01.html",
                              "created_at": "2026-07-20T00:00:00+00:00",
                              "trigger": "initial",
                              "verify_thread": "t1",
                              "verify_state": "clean",
                              "base": None,
                          }
                      ],
                  "takeoff": None,
                  "closed": None,
              }
          ]
  }
  plans_path.write_text(json.dumps(old_shape, indent=2), encoding="utf-8")

  # Trigger a save by approving the legacy plan.
  await plan_mgr.approve(meta.id, plan_id=1)

  raw = plans_path.read_text(encoding="utf-8")
  data = json.loads(raw)
  ver = data["plans"][0]["versions"][0]
  assert set(ver.keys()) == {"v", "file", "created_at", "trigger", "base"}
  assert "verify_thread" not in ver
  assert "verify_state" not in ver
  assert data["plans"][0]["takeoff"] is not None


# ---------------------------------------------------------------------------
# Broadcast
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_broadcast_called_on_present(tmp_path: Path) -> None:
  cfg, session_mgr, thread_mgr, _plan_mgr, meta = await _setup(tmp_path)
  plan_mgr = PlanRegistryManager(cfg, session_mgr, thread_mgr)
  calls: list[tuple[str, dict]] = []

  async def _fake_broadcast(session_id: str, event: dict) -> None:
    calls.append((session_id, event))

  session_mgr.broadcast_only = _fake_broadcast  # type: ignore[method-assign]
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")

  assert calls == [(meta.id, {"type": "plan_updated", "session_id": meta.id, "plan_id": 1})]


# ---------------------------------------------------------------------------
# Enum reservations
# ---------------------------------------------------------------------------


def test_derived_state_enum_reserves_zero_unknown() -> None:
  assert _DerivedState.UNKNOWN == 0
  assert _DerivedState.AWAITING_APPROVAL == 1
  assert _DerivedState.APPROVED == 2
  assert _DerivedState.SUPERSEDED == 3
  assert _DerivedState.ABANDONED == 4
