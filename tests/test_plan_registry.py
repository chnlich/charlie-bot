"""Tests for the plan registry: state machine, rejections, derived state, schema migration."""

import json
from pathlib import Path

import pytest
from conftest import make_plan_setup as _setup
from conftest import plan_doc, plan_page_html
from conftest import write_plan_artifact as _write_artifact
from conftest import write_stub_chrome as _write_stub_chrome

from src.core.plans import (
    PlanRegistryManager,
    _DerivedState,
    derive_state_str,
    read_plans_tolerant,
)

# ---------------------------------------------------------------------------
# Derived-state truth table (pure function of closed, takeoff)
# ---------------------------------------------------------------------------


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
        ({
            "as": "completed",
            "at": "2026-07-20T00:00:00+00:00"
        }, None, "completed"),
        (None, None, "awaiting approval"),
        (None, {
            "v": 1,
            "at": "2026-07-20T00:00:00+00:00"
        }, "approved"),
    ],
)
def test_derive_state_str_truth_table(closed, takeoff, expected) -> None:
  assert derive_state_str(plan_doc(closed=closed, takeoff=takeoff)) == expected


def test_derive_state_str_unknown_closed_as_raises() -> None:
  with pytest.raises(ValueError, match=r"unknown closed\.as"):
    derive_state_str(plan_doc(closed={"as": "weird", "at": "x"}))


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
  with pytest.raises(ValueError, match=r"--as must be superseded\|abandoned\|completed"):
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
  assert listing == {"plans": [], "errors": []}


def _write_legacy_verify_plans(plans_path: Path) -> None:
  """Write plans.json in the legacy shape: verify_thread/verify_state version keys the current schema drops."""
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


@pytest.mark.asyncio
async def test_old_plans_json_with_verify_keys_loads_and_list_omits_them(tmp_path: Path) -> None:
  """Migration: an old plans.json with verify_thread/verify_state keys loads fine, and list_plans
  output (and any later save) drops those legacy keys since the registry projects to the current schema."""
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  plans_path = cfg.sessions_dir / meta.id / "plans.json"
  _write_legacy_verify_plans(plans_path)

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
  _write_legacy_verify_plans(plans_path)

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
  cfg, session_mgr, _thread_mgr, _plan_mgr, meta = await _setup(tmp_path)
  plan_mgr = PlanRegistryManager(cfg, session_mgr)
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


# ---------------------------------------------------------------------------
# Tolerant read path (A1) — single authority in plans.py
# ---------------------------------------------------------------------------


def _good_plus_bad_plans_payload() -> dict:
  """One loadable plan (id 1, one initial version) and one that fails per-plan validation (id 2, empty versions)."""
  return {"plans": [plan_doc(title="Good"), plan_doc(2, [], title="Bad")]}


def test_read_plans_tolerant_missing_file(tmp_path: Path) -> None:
  result = read_plans_tolerant(tmp_path / "missing.json", "sess")
  assert result == {"plans": [], "errors": []}


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


def test_read_plans_tolerant_non_dict_top_level_returns_one_file_level_error(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  p.write_text("[1, 2, 3]", encoding="utf-8")
  result = read_plans_tolerant(p, "sess")
  assert result["plans"] == []
  assert len(result["errors"]) == 1
  assert result["errors"][0]["plan_id"] is None


def test_read_plans_tolerant_per_plan_failure_skips_bad_and_keeps_good(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  data = _good_plus_bad_plans_payload()
  p.write_text(json.dumps(data), encoding="utf-8")
  result = read_plans_tolerant(p, "sess")
  assert len(result["plans"]) == 1
  assert result["plans"][0]["id"] == 1
  assert result["plans"][0]["state"] == "awaiting approval"
  assert len(result["errors"]) == 1
  assert result["errors"][0]["plan_id"] == 2
  assert result["errors"][0]["session_id"] == "sess"


def test_read_plans_tolerant_unknown_closed_as_is_per_plan_error(tmp_path: Path) -> None:
  p = tmp_path / "plans.json"
  data = {
      "plans":
          [
              {
                  "id": 1,
                  "title": "Weird",
                  "versions": [{
                      "v": 1,
                      "file": "a.html",
                      "created_at": "x",
                      "trigger": "initial",
                      "base": None,
                  }],
                  "takeoff": None,
                  "closed": {
                      "as": "weird",
                      "at": "x"
                  },
              }
          ]
  }
  p.write_text(json.dumps(data), encoding="utf-8")
  result = read_plans_tolerant(p, "sess")
  assert result["plans"] == []
  assert len(result["errors"]) == 1
  assert result["errors"][0]["plan_id"] == 1


@pytest.mark.parametrize(
    "versions",
    [
        None,
        "not-a-list",
        [1, 2],
    ],
    ids=["null", "string", "list-of-non-dicts"],
)
def test_read_plans_tolerant_wrong_typed_versions_is_per_plan_error(tmp_path: Path, versions) -> None:
  """A plan with a wrong-typed ``versions`` field yields a per-plan error, never crashes the read."""
  p = tmp_path / "plans.json"
  data = {"plans": [{"id": 1, "title": "Bad", "versions": versions, "takeoff": None, "closed": None}]}
  p.write_text(json.dumps(data), encoding="utf-8")
  result = read_plans_tolerant(p, "sess")
  assert result["plans"] == []
  assert len(result["errors"]) == 1
  assert result["errors"][0]["plan_id"] == 1
  assert result["errors"][0]["session_id"] == "sess"
  assert isinstance(result["errors"][0]["error"], str) and result["errors"][0]["error"]


@pytest.mark.asyncio
async def test_list_plans_partial_degradation_returns_valid_plan_and_error(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  plans_path = cfg.sessions_dir / meta.id / "plans.json"
  plans_path.parent.mkdir(parents=True, exist_ok=True)
  data = _good_plus_bad_plans_payload()
  plans_path.write_text(json.dumps(data, indent=2), encoding="utf-8")

  listing = await plan_mgr.list_plans(meta.id)
  assert len(listing["plans"]) == 1
  assert listing["plans"][0]["id"] == 1
  assert listing["plans"][0]["state"] == "awaiting approval"
  assert len(listing["errors"]) == 1
  assert listing["errors"][0]["plan_id"] == 2


@pytest.mark.asyncio
async def test_present_fails_loud_on_corrupt_plans_json_and_does_not_reset(tmp_path: Path) -> None:
  """Reviewer A1: verbs keep failing loud on corrupt files; the file is not silently reset."""
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  plans_path = cfg.sessions_dir / meta.id / "plans.json"
  plans_path.parent.mkdir(parents=True, exist_ok=True)
  corrupt = "{not valid json"
  plans_path.write_text(corrupt, encoding="utf-8")

  with pytest.raises(json.JSONDecodeError):
    await plan_mgr.present(meta.id, file=f1, title="P1")
  # File is NOT silently reset — the corrupt content is preserved.
  assert plans_path.read_text(encoding="utf-8") == corrupt


# ---------------------------------------------------------------------------
# Path normalization at the verb boundary (A3)
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_present_normalizes_dot_slash_path_and_stores_canonical(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "x.html")
  dot_slash = "./" + file_rel

  await plan_mgr.present(meta.id, file=dot_slash, title="P1")

  data = json.loads((cfg.sessions_dir / meta.id / "plans.json").read_text(encoding="utf-8"))
  assert data["plans"][0]["versions"][0]["file"] == file_rel


@pytest.mark.asyncio
async def test_present_rejects_canonical_duplicate_after_dot_slash_present(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "x.html")
  dot_slash = "./" + file_rel
  await plan_mgr.present(meta.id, file=dot_slash, title="P1")

  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    await plan_mgr.present(meta.id, file=file_rel, title="P2")


@pytest.mark.asyncio
async def test_amend_rejects_canonical_duplicate_after_dot_slash_present(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "x.html")
  dot_slash = "./" + file_rel
  await plan_mgr.present(meta.id, file=dot_slash, title="P1")

  with pytest.raises(ValueError, match=r"already bound to plan 1 v1"):
    await plan_mgr.amend(meta.id, file=file_rel, plan_id=1)


# ---------------------------------------------------------------------------
# Amend trigger tightening (A4) — initial writable only by present
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_amend_rejects_initial_trigger(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  f2 = _write_artifact(cfg, meta.id, "plan_02.html")
  with pytest.raises(ValueError, match=r"trigger must be one of auto_amend\|feedback"):
    await plan_mgr.amend(meta.id, file=f2, plan_id=1, trigger="initial")


@pytest.mark.asyncio
async def test_present_stores_initial_trigger(tmp_path: Path) -> None:
  """present is the only writer of trigger=initial."""
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  f1 = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=f1, title="P1")
  data = json.loads((cfg.sessions_dir / meta.id / "plans.json").read_text(encoding="utf-8"))
  assert data["plans"][0]["versions"][0]["trigger"] == "initial"


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


@pytest.mark.asyncio
async def test_present_accepts_goal_at_budget(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html", content=_goal_doc("x" * 240))
  result = await plan_mgr.present(meta.id, file=file_rel, title="P1")
  assert result["state"] == "awaiting approval"


@pytest.mark.asyncio
async def test_goal_budget_counts_cjk_double_and_gates_amend(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  at_budget = _write_artifact(cfg, meta.id, "plan_01.html", content=_goal_doc("\u4e00" * 120))
  await plan_mgr.present(meta.id, file=at_budget, title="P1")
  over = _write_artifact(cfg, meta.id, "plan_02.html", content=_goal_doc("\u4e00" * 121))
  with pytest.raises(ValueError, match=r"242 weighted chars"):
    await plan_mgr.amend(meta.id, file=over)


@pytest.mark.asyncio
async def test_present_rejects_artifact_without_goal_section(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html", content="<html>plan</html>")
  with pytest.raises(ValueError, match="no 'Problem / Goal' section"):
    await plan_mgr.present(meta.id, file=file_rel, title="P1")


# ---------------------------------------------------------------------------
# Page budget gate: present/amend reject artifacts over the 1600 px height budget
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_page_budget_rejects_over_budget_naming_measured_height_and_gates_amend(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  at_budget = _write_artifact(cfg, meta.id, "plan_01.html")
  await plan_mgr.present(meta.id, file=at_budget, title="P1")
  cfg.headless_chrome_bin = _write_stub_chrome(tmp_path, 1601)
  over = _write_artifact(cfg, meta.id, "plan_02.html")
  with pytest.raises(ValueError, match=r"measures 1601 px as it opens: 1 px over the 1600 px budget"):
    await plan_mgr.amend(meta.id, file=over)


@pytest.mark.asyncio
async def test_present_rejects_when_headless_chrome_bin_unset(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  cfg.headless_chrome_bin = ""
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  with pytest.raises(ValueError, match="headless_chrome_bin is required for plan registration"):
    await plan_mgr.present(meta.id, file=file_rel, title="P1")


@pytest.mark.asyncio
async def test_present_rejects_failing_renderer_with_reason(tmp_path: Path) -> None:
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  stub = tmp_path / "stub-chrome-fail.sh"
  stub.write_text("#!/bin/sh\necho 'renderer exploded' >&2\nexit 1\n", encoding="utf-8")
  stub.chmod(0o755)
  cfg.headless_chrome_bin = str(stub)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html")
  with pytest.raises(ValueError, match="renderer exploded"):
    await plan_mgr.present(meta.id, file=file_rel, title="P1")


# ---------------------------------------------------------------------------
# DOM assertions: present/amend enforce the full plan assertion set, not just budgets
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_present_rejects_missing_section_number_naming_the_assertion(tmp_path: Path) -> None:
  """A page whose sections number 1,2,3,5,6,7 fails sections-numbered; one ValueError lists it."""
  broken = plan_page_html().replace('<span class="n">4</span>', '<span class="n">5</span>')
  assert '<span class="n">4</span>' not in broken
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html", content=broken)
  with pytest.raises(ValueError, match="sections-numbered"):
    await plan_mgr.present(meta.id, file=file_rel, title="P1")


@pytest.mark.asyncio
async def test_present_rejects_bare_ordinal_reference_naming_the_assertion(tmp_path: Path) -> None:
  """A plan page that names something outside it by a bare ordinal label fails ordinal-named; the
  registration gate refuses it through run_assertions with no gate change."""
  offending = plan_page_html().replace("</body></html>", "<p>计划 section 1 的措辞怎么收？</p></body></html>")
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html", content=offending)
  with pytest.raises(ValueError, match="ordinal-named"):
    await plan_mgr.present(meta.id, file=file_rel, title="P1")


@pytest.mark.asyncio
async def test_present_rejection_lists_every_failed_assertion(tmp_path: Path) -> None:
  """Deprived of its footer and numbered sections, one failure message names both defects."""
  broken = plan_page_html().replace('<span class="n">2</span>', '<span class="n">9</span>').replace(
      '<div class="foot"><p>How to respond.</p></div>', "")
  cfg, _session_mgr, _thread_mgr, plan_mgr, meta = await _setup(tmp_path)
  file_rel = _write_artifact(cfg, meta.id, "plan_01.html", content=broken)
  with pytest.raises(ValueError) as exc_info:
    await plan_mgr.present(meta.id, file=file_rel, title="P1")
  assert "sections-numbered" in str(exc_info.value)
  assert "foot-present" in str(exc_info.value)
