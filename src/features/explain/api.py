"""Session explain API: register a divider's explain task, read one entry, read every entry's status."""

import fastapi
import pydantic

from src.features.explain import explain
from src.infra import config, models, responses
from src.runtime import session_events
from src.runtime.api import deps

router = fastapi.APIRouter()


class ExplainRequest(pydantic.BaseModel):
  """One explain (btw-style) request for a divider: the chosen backend is required."""
  event_index: int
  backend: str


@router.post('/{session_id}/explain')
async def request_session_explain(
    session_id: str,
    body: ExplainRequest,
    _meta: models.SessionMetadata = fastapi.Depends(deps.require_session),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
) -> responses.FastJsonResponse:
  """Register (or return) the explain task for a divider; generation runs detached.

  A divider with no entry, or a terminal one (re-run overwrites), returns 202 with
  the fresh pending entry; a pending one returns 200 with the stored entry so one
  divider never runs a second concurrent generation.
  """
  option = cfg.get_backend_option(body.backend)
  if option is None:
    raise deps.bad_request(ValueError(f"unknown backend: {body.backend}"))
  entry, created = await explain.request_explain(events, session_id, body.event_index, option, cfg)
  return responses.FastJsonResponse(entry, status_code=202 if created else 200)


@router.get('/{session_id}/explain')
async def get_session_explain(
    session_id: str,
    upto: int,
    _meta: models.SessionMetadata = fastapi.Depends(deps.require_session),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
) -> responses.FastJsonResponse:
  """The single explain entry for a divider; answer and error bodies included."""
  entry = await explain.get_explain_entry(events, session_id, upto)
  if entry is None:
    raise fastapi.HTTPException(status_code=404, detail=f"no explain entry for event_index {upto}")
  return responses.FastJsonResponse(entry)


@router.get('/{session_id}/explain/status')
async def get_session_explain_status(
    session_id: str,
    _meta: models.SessionMetadata = fastapi.Depends(deps.require_session),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
) -> responses.FastJsonResponse:
  """Every explain entry's ``{upto: {state, backend, generated_at}}`` summary; bodies excluded.

  The chat page pulls this once per session load/switch to render each divider's
  explain button from persisted truth.
  """
  return responses.FastJsonResponse(await explain.explain_status(events, session_id))
