"""Internal API endpoints behind ``charliebot web reply`` and ``charliebot web dialog``."""

import asyncio

import fastapi
import pydantic

from src.features.web_reply import reply
from src.runtime import session_events, session_lifecycle, session_store
from src.runtime.api import deps

router = fastapi.APIRouter()


class WebReplyRequest(pydantic.BaseModel):
  """Request body for the internal web/reply endpoint: the calling session records *text* as its web reply."""
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  text: str


@router.post("/web/reply")
async def web_reply(
    req: WebReplyRequest,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    lifecycle: session_lifecycle.SessionLifecycle = fastapi.Depends(deps.get_session_lifecycle),
) -> dict:
  """Record the calling session's web reply and return the readback the CLI prints.

  Refusals map WebReplyError's status: 404 unknown session, 409 a thread session or a worker, 422 blank text.
  Nothing is recorded on a refusal.
  """
  try:
    return await reply.post_web_reply(req.session_id, req.text, store, events, lifecycle)
  except reply.WebReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.get("/web/dialog")
async def web_dialog(
    session_id: str,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
) -> dict:
  """Return the session's For-you layer, in log order; refusals map WebReplyError's status as the reply does."""
  try:
    await reply.require_web_session(store, session_id)
  except reply.WebReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc
  loaded = await asyncio.to_thread(events.load_chat_events_sync, session_id)
  return {"session_id": session_id, "entries": reply.for_you_entries(loaded)}
