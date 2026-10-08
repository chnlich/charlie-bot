"""Session explain API: register a divider's explain task, read one entry, read every entry's status."""

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel

from src.infra.config import CharlieBotConfig
from src.infra.models import SessionMetadata
from src.infra.responses import FastJsonResponse
from src.runtime.api.deps import bad_request, get_config_on_loop, get_session_manager, require_session
from src.runtime.sessions import SessionManager

router = APIRouter()


class ExplainRequest(BaseModel):
  """One explain (btw-style) request for a divider: the chosen backend is required."""
  event_index: int
  backend: str


@router.post('/{session_id}/explain')
async def request_session_explain(
    session_id: str,
    body: ExplainRequest,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> FastJsonResponse:
  """Register (or return) the explain task for a divider; generation runs detached.

  A divider with no entry, or a terminal one (re-run overwrites), returns 202 with
  the fresh pending entry; a pending one returns 200 with the stored entry so one
  divider never runs a second concurrent generation.
  """
  from src.features.explain import explain
  option = cfg.get_backend_option(body.backend)
  if option is None:
    raise bad_request(ValueError(f"unknown backend: {body.backend}"))
  entry, created = await explain.request_explain(session_mgr, session_id, body.event_index, option, cfg)
  return FastJsonResponse(entry, status_code=202 if created else 200)


@router.get('/{session_id}/explain')
async def get_session_explain(
    session_id: str,
    upto: int,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> FastJsonResponse:
  """The single explain entry for a divider; answer and error bodies included."""
  from src.features.explain import explain
  entry = await explain.get_explain_entry(session_mgr, session_id, upto)
  if entry is None:
    raise HTTPException(status_code=404, detail=f"no explain entry for event_index {upto}")
  return FastJsonResponse(entry)


@router.get('/{session_id}/explain/status')
async def get_session_explain_status(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> FastJsonResponse:
  """Every explain entry's ``{upto: {state, backend, generated_at}}`` summary; bodies excluded.

  The chat page pulls this once per session load/switch to render each divider's
  explain button from persisted truth.
  """
  from src.features.explain import explain
  return FastJsonResponse(await explain.explain_status(session_mgr, session_id))
