"""Plan registry API: the ``charliebot plan`` verbs (internal) and the session's plan listing.

``internal_router`` mounts under /api/internal and ``sessions_router`` under /api/sessions.
The one PlanRegistryManager of the process lives here, behind ``plan_manager`` (direct callers)
and ``get_plan_manager`` (the Depends form).
"""

from typing import Literal

from fastapi import APIRouter, Depends
from pydantic import BaseModel, ConfigDict

from src.features.artifacts.plans import PlanRegistryManager
from src.infra import config
from src.infra.models import SessionMetadata
from src.infra.responses import FastJsonResponse
from src.runtime.api.deps import bad_request, get_session_manager, require_found, require_session, session_manager
from src.runtime.sessions import SessionManager

internal_router = APIRouter()
sessions_router = APIRouter()


class PlanPresentRequest(BaseModel):
  """Request body for the internal plan/present endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  file: str
  title: str
  base_repo: str | None = None
  base_branch: str | None = None
  base_sha: str | None = None


PlanAmendTrigger = Literal["auto_amend", "feedback"]
PlanCloseMode = Literal["superseded", "abandoned", "completed"]


class PlanAmendRequest(BaseModel):
  """Request body for the internal plan/amend endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  file: str
  plan_id: int | None = None
  trigger: PlanAmendTrigger = "feedback"
  # Why this version differs from its predecessor; rides on the version record,
  # never in the page body. Required: the author is an agent absent at read time.
  note: str
  base_repo: str | None = None
  base_branch: str | None = None
  base_sha: str | None = None


class PlanApproveRequest(BaseModel):
  """Request body for the internal plan/approve endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  plan_id: int | None = None


class PlanCloseRequest(BaseModel):
  """Request body for the internal plan/close endpoint."""
  model_config = ConfigDict(extra="forbid")

  session_id: str
  plan_id: int
  close_as: PlanCloseMode


_plan_manager: PlanRegistryManager | None = None


def plan_manager() -> PlanRegistryManager:
  global _plan_manager
  if _plan_manager is None:
    _plan_manager = PlanRegistryManager(config.get_config(), session_manager())
  return _plan_manager


async def get_plan_manager() -> PlanRegistryManager:
  return plan_manager()


@sessions_router.get("/{session_id}/plans")
async def list_plans(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> FastJsonResponse:
  """Return the plan registry for a session with derived states and read errors.

  Unknown session → 404. Known session → always 200 with ``{"plans": [...], "errors": [...]}``;
  a corrupt registry produces 200 with empty plans and one error entry, never 5xx.
  """
  # The plan panel polls this route.
  return FastJsonResponse(await plan_mgr.list_plans(session_id))


def _build_base(req: PlanPresentRequest | PlanAmendRequest) -> dict | None:
  if req.base_repo is None and req.base_branch is None and req.base_sha is None:
    return None
  return {"repo": req.base_repo, "branch": req.base_branch, "sha": req.base_sha}


async def _authorize_plan_session(session_id: str, session_mgr: SessionManager) -> None:
  require_found(await session_mgr.get_session(session_id))


@internal_router.post("/plan/present")
async def plan_present(
    req: PlanPresentRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> dict:
  """Register a new plan lineage (v1, trigger=initial)."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.present(
        req.session_id,
        file=req.file,
        title=req.title,
        base=_build_base(req),
    )
  except ValueError as e:
    raise bad_request(e) from e


@internal_router.post("/plan/amend")
async def plan_amend(
    req: PlanAmendRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> dict:
  """Append the next version to a plan lineage."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.amend(
        req.session_id,
        file=req.file,
        plan_id=req.plan_id,
        trigger=req.trigger,
        base=_build_base(req),
        note=req.note,
    )
  except ValueError as e:
    raise bad_request(e) from e


@internal_router.post("/plan/approve")
async def plan_approve(
    req: PlanApproveRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> dict:
  """Record a takeoff against the latest version of a plan lineage."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.approve(req.session_id, plan_id=req.plan_id)
  except ValueError as e:
    raise bad_request(e) from e


@internal_router.post("/plan/close")
async def plan_close(
    req: PlanCloseRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> dict:
  """Terminate a plan lineage as superseded, abandoned, or completed."""
  await _authorize_plan_session(req.session_id, session_mgr)
  try:
    return await plan_mgr.close(req.session_id, req.plan_id, req.close_as)
  except ValueError as e:
    raise bad_request(e) from e
