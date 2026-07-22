"""Plan registry — per-session plan lineage state and derived-state derivation.

The registry owns three things per session: plan lineage (multiple plans, multiple versions
each), takeoff (the user approved a version), and closed (the lineage is terminated). It is a
display and management tool; it does not gate the master agent.
"""

import asyncio
import json
import os
from enum import IntEnum
from pathlib import Path
from typing import Optional

from src.core import plan_paths
from src.core.config import CharlieBotConfig
from src.core.models import utc_now
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

# ---------------------------------------------------------------------------
# DerivedState — internal enum (0 = UNKNOWN reserved); API use strings
# ---------------------------------------------------------------------------


class _DerivedState(IntEnum):
  UNKNOWN = 0
  AWAITING_APPROVAL = 1
  APPROVED = 2
  SUPERSEDED = 3
  ABANDONED = 4


_DERIVED_STATE_STR: dict[_DerivedState, str] = {
    _DerivedState.AWAITING_APPROVAL: "awaiting approval",
    _DerivedState.APPROVED: "approved",
    _DerivedState.SUPERSEDED: "superseded",
    _DerivedState.ABANDONED: "abandoned",
}


def _derive_state(closed: Optional[dict], takeoff: Optional[dict]) -> _DerivedState:
  """Pure function of (closed, takeoff) -> _DerivedState. Fail-loud."""
  if closed is not None:
    close_as = closed.get("as")
    if close_as == "superseded":
      return _DerivedState.SUPERSEDED
    if close_as == "abandoned":
      return _DerivedState.ABANDONED
    raise ValueError(f"unknown closed.as: {close_as!r}")
  if takeoff is None:
    return _DerivedState.AWAITING_APPROVAL
  return _DerivedState.APPROVED


def derive_state_str(plan: dict) -> str:
  """Return the derived-state display string for a plan dict (registry shape)."""
  versions = plan.get("versions") or []
  if not versions:
    raise ValueError(f"plan {plan.get('id')} has no versions")
  return _DERIVED_STATE_STR[_derive_state(plan.get("closed"), plan.get("takeoff"))]


def _utc_now_iso() -> str:
  return utc_now().isoformat()


# ---------------------------------------------------------------------------
# Schema projection — the canonical field set the registry persists and emits
# ---------------------------------------------------------------------------

_PLAN_FIELDS = ("id", "title", "versions", "takeoff", "closed")
_VERSION_FIELDS = ("v", "file", "created_at", "trigger", "base")


def _project_version(ver: dict) -> dict:
  return {k: ver.get(k) for k in _VERSION_FIELDS}


def _project_plan(plan: dict) -> dict:
  projected = {k: plan.get(k) for k in _PLAN_FIELDS}
  projected["versions"] = [_project_version(v) for v in plan.get("versions", [])]
  return projected


def _project_registry(data: dict) -> dict:
  """Project a raw loaded registry to the current schema (drops unknown keys on output)."""
  return {"plans": [_project_plan(p) for p in data.get("plans", [])]}


# ---------------------------------------------------------------------------
# Plan registry manager
# ---------------------------------------------------------------------------


class PlanRegistryManager:
  """Per-session plan registry: lineage state and version mutations."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager, thread_mgr: ThreadManager):
    self._cfg = cfg
    self._session_mgr = session_mgr
    self._thread_mgr = thread_mgr
    self._locks: dict[str, asyncio.Lock] = {}

  # -- locking ------------------------------------------------------------

  def _lock_for(self, session_id: str) -> asyncio.Lock:
    lock = self._locks.get(session_id)
    if lock is None:
      lock = asyncio.Lock()
      self._locks[session_id] = lock
    return lock

  # -- persistence --------------------------------------------------------

  def _plans_path(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id / "plans.json"

  async def _load(self, session_id: str) -> dict:
    """Load the raw registry dict. Unknown legacy keys are tolerated.

    The plain-dict loader preserves whatever is on disk; projection to the current schema
    happens on save and on list_plans output, never on load, so untouched files are not
    rewritten unnecessarily.
    """
    path = self._plans_path(session_id)
    if not path.exists():
      return {"plans": []}
    raw = await asyncio.to_thread(path.read_text, "utf-8")
    return json.loads(raw)

  async def _save(self, session_id: str, data: dict) -> None:
    path = self._plans_path(session_id)
    content = json.dumps(_project_registry(data), indent=2)
    await asyncio.to_thread(self._atomic_write, path, content)

  @staticmethod
  def _atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(content, encoding="utf-8")
    os.replace(tmp, path)

  async def _broadcast(self, session_id: str, plan_id: int) -> None:
    await self._session_mgr.broadcast_only(
        session_id, {
            "type": "plan_updated",
            "session_id": session_id,
            "plan_id": plan_id,
        })

  # -- helpers -----------------------------------------------------------

  def _validate_file_in_session_dir(self, session_id: str, file: str) -> str:
    session_dir = self._cfg.sessions_dir / session_id
    candidate, relative = plan_paths.resolve_plan_file(session_dir, file)
    if relative is None:
      raise ValueError(f"file {file!r} resolves outside the session directory")
    if not candidate.exists():
      raise ValueError(f"file {file!r} not found inside the session directory")
    return relative.as_posix()

  def _find_binding_by_file(self, session_id: str, data: dict, file: str) -> Optional[tuple[int, int]]:
    session_dir = self._cfg.sessions_dir / session_id
    for plan in data["plans"]:
      for ver in plan["versions"]:
        _candidate, relative = plan_paths.resolve_plan_file(session_dir, ver["file"])
        if relative is not None and relative.as_posix() == file:
          return plan["id"], ver["v"]
    return None

  def _get_plan(self, data: dict, plan_id: int) -> Optional[dict]:
    for plan in data["plans"]:
      if plan["id"] == plan_id:
        return plan
    return None

  # -- verbs --------------------------------------------------------------

  async def present(
      self,
      session_id: str,
      file: str,
      title: str,
      base: Optional[dict] = None,
  ) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      file_relative = self._validate_file_in_session_dir(session_id, file)
      existing_file = self._find_binding_by_file(session_id, data, file_relative)
      if existing_file is not None:
        raise ValueError(f"file {file!r} already bound to plan {existing_file[0]} v{existing_file[1]}")
      next_id = max((p["id"] for p in data["plans"]), default=0) + 1
      plan = {
          "id": next_id,
          "title": title,
          "versions":
              [{
                  "v": 1,
                  "file": file_relative,
                  "created_at": _utc_now_iso(),
                  "trigger": "initial",
                  "base": base,
              }],
          "takeoff": None,
          "closed": None,
      }
      data["plans"].append(plan)
      await self._save(session_id, data)
    await self._broadcast(session_id, next_id)
    return {"plan": next_id, "v": 1, "state": derive_state_str(plan)}

  async def amend(
      self,
      session_id: str,
      file: str,
      plan_id: Optional[int] = None,
      trigger: str = "feedback",
      base: Optional[dict] = None,
  ) -> dict:
    if trigger not in ("initial", "auto_amend", "feedback"):
      raise ValueError(f"trigger must be one of initial|auto_amend|feedback, got {trigger!r}")
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      file_relative = self._validate_file_in_session_dir(session_id, file)
      existing_file = self._find_binding_by_file(session_id, data, file_relative)
      if existing_file is not None:
        raise ValueError(f"file {file!r} already bound to plan {existing_file[0]} v{existing_file[1]}")
      plan = self._resolve_target_plan_for_amend(data, plan_id)
      new_v = max(ver["v"] for ver in plan["versions"]) + 1
      plan["versions"].append(
          {
              "v": new_v,
              "file": file_relative,
              "created_at": _utc_now_iso(),
              "trigger": trigger,
              "base": base,
          })
      plan["takeoff"] = None
      await self._save(session_id, data)
    await self._broadcast(session_id, plan["id"])
    return {"plan": plan["id"], "v": new_v, "state": derive_state_str(plan)}

  def _resolve_target_plan_for_amend(self, data: dict, plan_id: Optional[int]) -> dict:
    if plan_id is not None:
      plan = self._get_plan(data, plan_id)
      if plan is None:
        raise ValueError(f"plan {plan_id} not found in session")
      if plan.get("closed") is not None:
        raise ValueError(f"plan {plan_id} is closed ({plan['closed']['as']!r})")
      return plan
    open_candidates = [p for p in data["plans"] if p.get("closed") is None and p.get("takeoff") is None]
    if not open_candidates:
      raise ValueError("no open lineage to amend; pass --plan to amend an approved one")
    if len(open_candidates) > 1:
      ids = ", ".join(str(p["id"]) for p in open_candidates)
      raise ValueError(f"amend requires --plan (multiple open lineages: {ids})")
    return open_candidates[0]

  async def approve(self, session_id: str, plan_id: Optional[int] = None) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      plan = self._resolve_target_plan_for_approve(data, plan_id)
      latest = plan["versions"][-1]
      plan["takeoff"] = {"v": latest["v"], "at": _utc_now_iso()}
      await self._save(session_id, data)
    await self._broadcast(session_id, plan["id"])
    return {"plan": plan["id"], "v": latest["v"], "state": derive_state_str(plan)}

  def _resolve_target_plan_for_approve(self, data: dict, plan_id: Optional[int]) -> dict:
    if plan_id is not None:
      plan = self._get_plan(data, plan_id)
      if plan is None:
        raise ValueError(f"plan {plan_id} not found in session")
      if plan.get("closed") is not None:
        raise ValueError(f"plan {plan_id} is closed ({plan['closed']['as']!r})")
      if plan.get("takeoff") is not None:
        raise ValueError(f"plan {plan_id} is already approved")
      return plan
    candidates = [p for p in data["plans"] if p.get("closed") is None and p.get("takeoff") is None]
    if not candidates:
      raise ValueError("no plan available for approval")
    if len(candidates) > 1:
      ids = ", ".join(str(p["id"]) for p in candidates)
      raise ValueError(f"approve requires --plan (multiple candidates: {ids})")
    return candidates[0]

  async def close(self, session_id: str, plan_id: int, close_as: str) -> dict:
    if close_as not in ("superseded", "abandoned"):
      raise ValueError(f"--as must be superseded|abandoned, got {close_as!r}")
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      plan = self._get_plan(data, plan_id)
      if plan is None:
        raise ValueError(f"plan {plan_id} not found in session")
      if plan.get("closed") is not None:
        raise ValueError(f"plan {plan_id} is already closed ({plan['closed']['as']!r})")
      plan["closed"] = {"as": close_as, "at": _utc_now_iso()}
      await self._save(session_id, data)
    await self._broadcast(session_id, plan_id)
    return {"plan": plan_id, "state": derive_state_str(plan)}

  async def list_plans(self, session_id: str) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
    return self._with_derived_states(data)

  @staticmethod
  def _with_derived_states(data: dict) -> dict:
    plans = []
    for plan in data.get("plans", []):
      projected = _project_plan(plan)
      enriched = {**projected, "state": derive_state_str(projected)}
      plans.append(enriched)
    return {"plans": plans}
