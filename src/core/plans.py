"""Plan registry — per-session plan lineage state and derived-state derivation.

The registry owns three things per session: plan lineage (multiple plans, multiple versions
each), takeoff (the user approved a version), and closed (the lineage is terminated). It is a
display and management tool; it does not gate the master agent.

Error domains are split: verbs (present/amend/approve/close) read plans.json via the strict
``_load`` and fail loud on corrupt files. All read surfaces (list endpoint, sidebar probe)
consume the tolerant read in ``read_plans_tolerant`` — the single authority for catch-and-derive.
"""

import asyncio
import json
import posixpath
from enum import IntEnum
from pathlib import Path

import structlog

from src.core import plan_paths
from src.core.artifact_check import run_assertions
from src.core.config import CharlieBotConfig
from src.core.json_utils import write_json_atomically
from src.core.models import utc_now
from src.core.sessions import SessionManager
from src.core.sidebar_state import mark_sidebar_dirty

log = structlog.get_logger()

# ---------------------------------------------------------------------------
# DerivedState — internal enum (0 = UNKNOWN reserved); API use strings
# ---------------------------------------------------------------------------


class _DerivedState(IntEnum):
  UNKNOWN = 0
  AWAITING_APPROVAL = 1
  APPROVED = 2
  SUPERSEDED = 3
  ABANDONED = 4
  COMPLETED = 5


_DERIVED_STATE_STR: dict[_DerivedState, str] = {
    _DerivedState.AWAITING_APPROVAL: "awaiting approval",
    _DerivedState.APPROVED: "approved",
    _DerivedState.SUPERSEDED: "superseded",
    _DerivedState.ABANDONED: "abandoned",
    _DerivedState.COMPLETED: "completed",
}


def _derive_state(closed: dict | None, takeoff: dict | None) -> _DerivedState:
  """Pure function of (closed, takeoff) -> _DerivedState. Fail-loud.

  Wrong types (non-dict closed/takeoff) raise ValueError so the tolerant read's
  ``except (OSError, ValueError)`` can attribute them to a single plan instead of
  aborting the whole listing. Verbs never pass wrong-typed data here — they construct
  the dicts themselves.
  """
  if closed is not None:
    if not isinstance(closed, dict):
      raise ValueError(f"closed must be a dict or None, got {type(closed).__name__}")
    close_as = closed.get("as")
    if close_as == "superseded":
      return _DerivedState.SUPERSEDED
    if close_as == "abandoned":
      return _DerivedState.ABANDONED
    if close_as == "completed":
      return _DerivedState.COMPLETED
    raise ValueError(f"unknown closed.as: {close_as!r}")
  if takeoff is None:
    return _DerivedState.AWAITING_APPROVAL
  if not isinstance(takeoff, dict):
    raise ValueError(f"takeoff must be a dict or None, got {type(takeoff).__name__}")
  return _DerivedState.APPROVED


def derive_state_str(plan: dict) -> str:
  """Return the derived-state display string for a plan dict (registry shape)."""
  if not isinstance(plan, dict):
    raise ValueError(f"plan must be a dict, got {type(plan).__name__}")
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
  if not isinstance(ver, dict):
    raise ValueError(f"version must be a dict, got {type(ver).__name__}")
  return {k: ver.get(k) for k in _VERSION_FIELDS}


def _project_plan(plan: dict) -> dict:
  if not isinstance(plan, dict):
    raise ValueError(f"plan must be a dict, got {type(plan).__name__}")
  projected = {k: plan.get(k) for k in _PLAN_FIELDS}
  versions = plan.get("versions", [])
  if not isinstance(versions, list):
    raise ValueError(f"versions must be a list, got {type(versions).__name__}")
  projected["versions"] = [_project_version(v) for v in versions]
  return projected


def _project_registry(data: dict) -> dict:
  """Project a raw loaded registry to the current schema (drops unknown keys on output)."""
  return {"plans": [_project_plan(p) for p in data.get("plans", [])]}


# ---------------------------------------------------------------------------
# Tolerant read — single authority for listing/probe surfaces
# ---------------------------------------------------------------------------


def read_plans_tolerant(plans_path: Path, session_id: str) -> dict:
  """Tolerant read of a session's plans.json.

  Returns ``{"plans": [<projected plan enriched with "state">...], "errors": [<entry>...]}``.

  - Missing file: ``plans: []``, ``errors: []``.
  - Unreadable file / invalid JSON / non-dict top level: ``plans: []``, one error entry.
  - Per-plan derive failure (empty versions, unknown closed.as, wrong types): skip that
    plan, append an error entry; remaining plans are still returned enriched.

  Error entry fields: ``session_id``, ``plan_id`` (null for file-level errors), ``error`` (str).

  Catches exactly ``(OSError, ValueError)`` — ``json.JSONDecodeError`` is a ``ValueError``
  subclass. Never raises for expected corruption; the caller may iterate the result directly.
  """
  errors: list[dict] = []
  if not plans_path.exists():
    return {"plans": [], "errors": errors}
  try:
    raw = plans_path.read_text(encoding="utf-8")
    data = json.loads(raw)
  except (OSError, ValueError) as e:
    return {
        "plans": [],
        "errors": [{
            "session_id": session_id,
            "plan_id": None,
            "error": str(e)
        }],
    }
  if not isinstance(data, dict):
    return {
        "plans": [],
        "errors":
            [
                {
                    "session_id": session_id,
                    "plan_id": None,
                    "error": f"registry top level is {type(data).__name__}, expected dict",
                }
            ],
    }
  plans_out: list[dict] = []
  for plan in data.get("plans", []):
    try:
      projected = _project_plan(plan)
      enriched = {**projected, "state": derive_state_str(projected)}
      plans_out.append(enriched)
    except (OSError, ValueError) as e:
      plan_id = plan.get("id") if isinstance(plan, dict) else None
      errors.append({"session_id": session_id, "plan_id": plan_id, "error": str(e)})
  return {"plans": plans_out, "errors": errors}


# ---------------------------------------------------------------------------
# Plan registry manager
# ---------------------------------------------------------------------------


class PlanRegistryManager:
  """Per-session plan registry: lineage state and version mutations."""

  def __init__(self, cfg: CharlieBotConfig, session_mgr: SessionManager):
    self._cfg = cfg
    self._session_mgr = session_mgr
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
    rewritten unnecessarily. Fails loud on corrupt files (verbs only).
    """
    path = self._plans_path(session_id)
    if not path.exists():
      return {"plans": []}
    raw = await asyncio.to_thread(path.read_text, "utf-8")
    return json.loads(raw)

  async def _save(self, session_id: str, data: dict) -> None:
    # Every save path first reads plans.json or a bound artifact out of the
    # session dir, so the dir exists; no mkdir here — a deleted dir must fail
    # this write, not resurrect.
    await asyncio.to_thread(
        write_json_atomically, self._plans_path(session_id), _project_registry(data), indent=2)
    # Single funnel for every registry verb (present/amend/approve/close):
    # an awaiting-approval change must reach the sidebar snapshot.
    mark_sidebar_dirty(session_id)

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

  def _find_binding_by_file(self, session_id: str, data: dict, file: str) -> tuple[int, int] | None:
    """Return (plan_id, v) for the first version whose normalized file path equals ``file``.

    Both sides are normalized via ``posixpath.normpath`` so legacy rows with non-canonical
    spellings (e.g. ``./artifacts/c.html``) match a canonical target without any data migration.
    """
    for plan in data["plans"]:
      for ver in plan["versions"]:
        if posixpath.normpath(ver["file"]) == file:
          return plan["id"], ver["v"]
    return None

  def _validate_new_version_file(self, session_id: str, file: str, data: dict) -> str:
    """Validate *file* as a new plan version's binding and return its session-relative path.

    The file must live inside the session directory, pass the plan assertion set
    (exactly what ``charliebot artifact check --genre plan`` enforces), and not already back
    an existing plan version. Callers hold the session lock and pass the freshly loaded
    registry *data*.
    """
    file = posixpath.normpath(file)
    file_relative = self._validate_file_in_session_dir(session_id, file)
    artifact = self._cfg.sessions_dir / session_id / file_relative
    failures = [o for o in run_assertions("plan", artifact, self._cfg) if not o.passed]
    if failures:
      reasons = "; ".join(f"{o.name}: {o.detail}" for o in failures)
      raise ValueError(
          f"plan artifact fails {len(failures)} check assertion(s): {reasons}. "
          "Recover headroom by folding, per the page-budget rules in the BLOCK KIT comment of "
          "prompts/plan_template.html. Measure locally with: "
          "charliebot artifact check <artifact.html> --genre plan")
    existing_file = self._find_binding_by_file(session_id, data, file_relative)
    if existing_file is not None:
      raise ValueError(f"file {file!r} already bound to plan {existing_file[0]} v{existing_file[1]}")
    return file_relative

  def _get_plan(self, data: dict, plan_id: int) -> dict | None:
    for plan in data["plans"]:
      if plan["id"] == plan_id:
        return plan
    return None

  def _require_open_plan(self, data: dict, plan_id: int) -> dict:
    """Return the named plan, raising when it is missing or closed."""
    plan = self._get_plan(data, plan_id)
    if plan is None:
      raise ValueError(f"plan {plan_id} not found in session")
    if plan.get("closed") is not None:
      raise ValueError(f"plan {plan_id} is closed ({plan['closed']['as']!r})")
    return plan

  # -- verbs --------------------------------------------------------------

  async def present(
      self,
      session_id: str,
      file: str,
      title: str,
      base: dict | None = None,
  ) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      file_relative = self._validate_new_version_file(session_id, file, data)
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
      plan_id: int | None = None,
      trigger: str = "feedback",
      base: dict | None = None,
  ) -> dict:
    if trigger not in ("auto_amend", "feedback"):
      raise ValueError(f"trigger must be one of auto_amend|feedback, got {trigger!r}")
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      file_relative = self._validate_new_version_file(session_id, file, data)
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

  def _resolve_target_plan_for_amend(self, data: dict, plan_id: int | None) -> dict:
    if plan_id is not None:
      return self._require_open_plan(data, plan_id)
    open_candidates = [p for p in data["plans"] if p.get("closed") is None and p.get("takeoff") is None]
    if not open_candidates:
      raise ValueError("no open lineage to amend; pass --plan to amend an approved one")
    if len(open_candidates) > 1:
      ids = ", ".join(str(p["id"]) for p in open_candidates)
      raise ValueError(f"amend requires --plan (multiple open lineages: {ids})")
    return open_candidates[0]

  async def approve(self, session_id: str, plan_id: int | None = None) -> dict:
    async with self._lock_for(session_id):
      data = await self._load(session_id)
      plan = self._resolve_target_plan_for_approve(data, plan_id)
      latest = plan["versions"][-1]
      plan["takeoff"] = {"v": latest["v"], "at": _utc_now_iso()}
      await self._save(session_id, data)
    await self._broadcast(session_id, plan["id"])
    return {"plan": plan["id"], "v": latest["v"], "state": derive_state_str(plan)}

  def _resolve_target_plan_for_approve(self, data: dict, plan_id: int | None) -> dict:
    if plan_id is not None:
      plan = self._require_open_plan(data, plan_id)
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
    if close_as not in ("superseded", "abandoned", "completed"):
      raise ValueError(f"--as must be superseded|abandoned|completed, got {close_as!r}")
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
    """Tolerant read of the registry. Returns ``{"plans": [...], "errors": [...]}``.

    The strict ``_load`` used by verbs is unchanged; this is the only read path for listing
    and probe surfaces. Errors are returned (never raised) so a corrupt single-session file
    cannot 5xx the sidebar poll for all sessions.
    """
    plans_path = self._plans_path(session_id)
    return await asyncio.to_thread(read_plans_tolerant, plans_path, session_id)
