"""Internal API endpoints behind ``charliebot web reply``, ``charliebot web dialog`` and ``charliebot web status``."""

import asyncio
import pathlib

import fastapi
import pydantic

from src.features.web_reply import reply, status
from src.infra import config
from src.runtime import (
    control_events,
    file_urls,
    run_token,
    session_events,
    session_lifecycle,
    session_store,
    task_sessions,
)
from src.runtime.api import deps

router = fastapi.APIRouter()


def _file_link_scan_context(
    cfg: config.CharlieBotConfig, store: session_store.SessionStore, session_id: str) -> file_urls.ScanContext:
  return file_urls.ScanContext(
      cfg.server.port,
      cfg.charliebot_home,
      store.session_dir(session_id),
      pathlib.Path.home(),
  )


class WebReplyRequest(pydantic.BaseModel):
  """Request body for the internal web/reply endpoint: the calling session records *text* as its web reply.

  *keep_language* is the agent's reason for posting *text* past the language check; None when the agent gave none.
  """
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  text: str
  keep_language: str | None = None


class WebStatusRequest(pydantic.BaseModel):
  """Request body for the internal web/status endpoints: the calling session and *text*, a status file's JSON."""
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  text: str


class WebStatusConfirmRequest(pydantic.BaseModel):
  """Request body for the internal web/status/confirm endpoint: the calling session."""
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str


class WebAskCheckRequest(pydantic.BaseModel):
  """Request body for the internal web/ask-check endpoint: one cold read of a Waiting on you item."""
  model_config = pydantic.ConfigDict(extra="forbid")

  session_id: str
  item: dict
  backend: str
  answer: str


@router.post("/web/reply")
async def web_reply(
    req: WebReplyRequest,
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    lifecycle: session_lifecycle.SessionLifecycle = fastapi.Depends(deps.get_session_lifecycle),
    task_mgr: task_sessions.TaskTreeManager = fastapi.Depends(deps.get_task_manager),
    caller: run_token.CallerIdentity = fastapi.Depends(deps.require_caller),
) -> dict:
  """Record the calling session's web reply and return the readback the CLI prints.

  Refusals map WebReplyError's status: 404 unknown session, 409 a thread session, a worker or a language hold, 422
  blank text, an empty keep_language reason or a dead file link. Nothing is recorded on a refusal.
  """
  try:
    request_id = None
    if caller.claims is not None:
      request_id = control_events.derived_agent_send_request_id(
          caller.claims.run_id, "web reply", req.session_id, req.text)
    return await reply.post_web_reply(
        req.session_id,
        req.text,
        store,
        events,
        lifecycle,
        keep_language=req.keep_language,
        request_id=request_id,
        tree=task_mgr,
        scan_context=_file_link_scan_context(cfg, store, req.session_id))
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


@router.post("/web/status")
async def web_status_set(
    req: WebStatusRequest,
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    tree: task_sessions.TaskTreeManager = fastapi.Depends(deps.get_task_manager),
) -> dict:
  """Store the calling session's status items and return the stored status the CLI prints.

  Refusals map WebReplyError's status: 404 unknown session, 409 a thread session, a worker, a stale plan link or an
  unread waiting item, and 422 a file that is not a JSON object of valid items, carries a server-written field, has a
  waiting item without context or links a dead file. Nothing is stored or recorded on a refusal.
  """
  try:
    return await status.set_status(
        req.session_id, req.text, store, events, tree, scan_context=_file_link_scan_context(cfg, store, req.session_id))
  except reply.WebReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/web/status/confirm")
async def web_status_confirm(
    req: WebStatusConfirmRequest,
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
) -> dict:
  """Confirm the calling session's stored status and return it.

  Refusals map WebReplyError's status: 404 unknown session, 409 a thread session, a worker or a stale plan link. A
  confirmation records one event and cold reads nothing.
  """
  try:
    return await status.confirm_status(
        req.session_id, store, events, scan_context=_file_link_scan_context(cfg, store, req.session_id))
  except reply.WebReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/web/status/unread")
async def web_status_unread(
    req: WebStatusRequest,
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
) -> dict:
  """Return the status file's waiting items that no cold read of this session has read as they stand.

  The file parses and validates as a set's does, including the waiting-item context and file-link checks; refusals
  match the set's. Nothing is stored or recorded.
  """
  try:
    return await status.unread_waiting(
        req.session_id, req.text, store, events, scan_context=_file_link_scan_context(cfg, store, req.session_id))
  except reply.WebReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.post("/web/ask-check")
async def web_ask_check(
    req: WebAskCheckRequest,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
) -> dict:
  """Record one cold read of a Waiting on you item and return its digest.

  Refusals map WebReplyError's status: 404 unknown session, 409 a thread session or a worker, 422 an invalid item.
  Nothing is recorded on a refusal.
  """
  try:
    return await status.record_ask_check(req.session_id, req.item, req.backend, req.answer, store, events)
  except reply.WebReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc


@router.get("/web/status")
async def web_status_read(
    session_id: str,
    store: session_store.SessionStore = fastapi.Depends(deps.get_session_store),
    tree: task_sessions.TaskTreeManager = fastapi.Depends(deps.get_task_manager),
) -> dict:
  """Return the session's status and the open descendants that wait on the user; refusals as the set's."""
  try:
    return await status.read_status(session_id, store, tree)
  except reply.WebReplyError as exc:
    raise fastapi.HTTPException(status_code=exc.status, detail=exc.detail) from exc
