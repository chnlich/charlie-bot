"""Chat API routes — triggers master CC process, returns 202 Accepted."""

from pathlib import Path
from typing import Any

import aiofiles
from fastapi import APIRouter, Depends, File, HTTPException, UploadFile
from fastapi.responses import JSONResponse

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.deferred import deferred_import_loader, deferred_module_getattr
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SendMessageRequest, SessionMetadata, SessionStatus
from src.infra.tasks import create_logged_task
from src.runtime.agent_process.base import make_master_done_event
from src.runtime.api.deps import (
    get_config_on_loop,
    get_session_manager,
    get_task_manager,
    require_caller,
    require_found,
    require_session,
)
from src.runtime.api.message_utils import build_agent_input_content
from src.runtime.message_events import serialize_uploaded_files
from src.runtime.run_token import CallerIdentity
from src.runtime.runs import RunIdentityConflictError
from src.runtime.session_dispatch import agent_provenance, input_event_type_for_caller
from src.runtime.sessions import SessionManager
from src.runtime.task_sessions import TaskTreeManager

log = LazyStructlogLogger()

router = APIRouter()

_load_cancel_master = deferred_import_loader("cancel_master", "src.runtime.master_cc_queue")


def __getattr__(name: str) -> Any:
  # The "src.runtime.api.chat.cancel_master" patch target resolves through this hook.
  return deferred_module_getattr(name, __name__, globals(), "cancel_master", _load_cancel_master)


@router.post("/{session_id}/upload")
async def upload_file(
    session_id: str,
    file: UploadFile = File(...),
    _meta: SessionMetadata = Depends(require_session),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Upload a file to the session's uploads directory. Returns {filename, path, size}."""
  uploads_dir = cfg.sessions_dir / session_id / "uploads"
  uploads_dir.mkdir(parents=True, exist_ok=True)

  dest = uploads_dir / Path(file.filename or "upload").name
  size = 0
  try:
    async with aiofiles.open(dest, "wb") as out:
      while True:
        chunk = await file.read(1024 * 1024)  # 1 MB chunks
        if not chunk:
          break
        await out.write(chunk)
        size += len(chunk)
  except Exception as e:
    log.warning("file_upload_failed", session=session_id, filename=file.filename, error=str(e))
    raise HTTPException(status_code=500, detail="Failed to save uploaded file") from e

  log.info("file_uploaded", session=session_id, filename=file.filename, size=size)
  return {"filename": file.filename, "path": str(dest.resolve()), "size": size}


@router.post("/{session_id}/message")
async def send_message(
    session_id: str,
    req: SendMessageRequest,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> JSONResponse:
  """Send a message to the master CC agent. Returns 202; response streams via WebSocket."""
  # A v2 task node routes input through the durable dispatcher: browser and
  # operator input are real user input, a run-token agent on the same route
  # stays agent input with its own provenance. An archived node refuses the
  # agent input with 409 and restores for the user's own message before the
  # admission; the executor seam (next stage) decides any launch. The response
  # carries the dispatcher's decision.
  if meta.profile is not None:
    from src.runtime.task_sessions import TaskConflictError, TaskForbiddenError, TaskInvalidError

    event_type = input_event_type_for_caller(caller)
    from_session, from_session_name = agent_provenance(caller) if event_type != ET.USER else (None, None)
    uploaded_files = serialize_uploaded_files(req.uploaded_files)
    try:
      admitted = await task_mgr.dispatch.admit_input(
          session_id,
          event_type=event_type,
          content=req.content,
          actor="user" if event_type == ET.USER else "agent",
          uploaded_files=uploaded_files,
          from_session=from_session,
          from_session_name=from_session_name,
      )
      decision = await task_mgr.dispatch.dispatch_pending(session_id)
    except (TaskConflictError, TaskForbiddenError, TaskInvalidError) as e:
      from src.runtime.api.sessions import _task_http_error
      raise _task_http_error(e) from e
    return JSONResponse(
        status_code=202,
        content={
            "status": "accepted",
            "input_event_id": str(admitted.get("id")),
            "launch": bool(decision.get("launch")),
            **({
                "reason": decision["reason"]
            } if decision.get("reason") else {}),
        })

  # The only content path that does not go through trigger_master: unarchive an
  # archived target here, before dispatching, so the dispatch below sees the
  # restored session.
  if meta.status == SessionStatus.ARCHIVED:
    meta = require_found(await session_mgr.unarchive_session(session_id))

  uploaded_files = serialize_uploaded_files(req.uploaded_files)
  content = build_agent_input_content(req.content, uploaded_files)

  log.info(
      "send_message",
      session=session_id,
      content_chars=len(content),
      uploaded_files_count=len(uploaded_files),
  )

  # Fire-and-forget: spawn master CC in a background task
  create_logged_task(
      run_and_finalize(
          cfg,
          meta,
          content,
          session_mgr,
          display_content=req.content,
          uploaded_files=uploaded_files,
          is_voice=req.is_voice))

  return JSONResponse(status_code=202, content={"status": "accepted"})


async def _cancel_task_node_runs(session_id: str) -> int:
  """Stop every launched, non-terminal Run of one task-tree node.

  Each stop rides the run store's own entry point: the durable request lands
  first, then the identity-checked signal and exit observation, so a run
  re-attached after a server restart stops the same way. The request id is
  derived from the run id, so a repeated press is the same idempotent request.
  Returns how many stops were requested.
  """
  task_mgr = await get_task_manager()
  events = task_mgr.runs.load_events_sync(session_id)
  requested = 0
  for run in task_mgr.runs.list_run_records_sync(session_id):
    if run.pid is None or task_mgr.runs.run_has_terminal_fact(run, events):
      continue  # queued runs launch nothing; finished runs keep their outcome
    result = await task_mgr.runs.request_stop(session_id, run.id, f"chat-cancel:{run.id}")
    if result.stop_requested:
      requested += 1
  return requested


@router.post("/{session_id}/cancel")
async def cancel_master_agent(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> dict:
  """Stop the session's current execution.

  A task-tree node (the metadata carries a profile) stops its launched Runs
  through the run store; a legacy session keeps the master-cancel path. With
  nothing running, both keep today's error broadcast and 404.
  """
  if meta.profile is not None:
    try:
      requested = await _cancel_task_node_runs(session_id)
    except RunIdentityConflictError as e:
      from src.runtime.api.sessions import _task_http_error
      raise _task_http_error(e) from e
    found = requested > 0
  else:
    found = await _load_cancel_master(globals())(session_id, meta=meta, session_mgr=session_mgr)
  if not found:
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": ET.ASSISTANT_ERROR,
            "content": "No active master agent to cancel.",
        })
    raise HTTPException(status_code=404, detail="No active master agent")
  return {"ok": True}


async def run_and_finalize(
    cfg: CharlieBotConfig,
    meta: SessionMetadata,
    content: str,
    session_mgr: SessionManager,
    *,
    skip_user_event: bool = False,
    display_content: str | None = None,
    uploaded_files: list[dict] | None = None,
    is_voice: bool = False,
) -> None:
  """Run master CC; the consumer owns cc_session_id persistence and naming."""
  log.info("run_and_finalize_start", session=meta.id, backend=meta.backend)
  backend_id = meta.backend
  backend_option = cfg.get_backend_option(backend_id)
  # lazy: keeps the master-turn chain off the M99 server import floor (docs/perf_baseline.md@5175adf09)
  from src.runtime import master_cc_queue
  try:
    await master_cc_queue.run_message(
        cfg,
        meta,
        content,
        session_mgr.callbacks(),
        ET.USER,
        skip_user_event=skip_user_event,
        backend_option=backend_option,
        display_content=display_content,
        uploaded_files=uploaded_files,
        is_voice=is_voice,
    )
    # cc_session_id persistence is owned by the consumer in run_message; nothing
    # downstream here reads meta.cc_session_id.
  except Exception as e:
    log.exception("master_cc_run_failed", session=meta.id)
    # run_message() should handle and emit failures, but keep this as a
    # last-resort guard so the UI never gets stuck in "Thinking...".
    error_event = {"type": ET.ASSISTANT_ERROR, "content": f"Agent error: {e}"}
    done_event = make_master_done_event(1, still_thinking=False)
    await session_mgr.persist_and_broadcast(meta.id, error_event)
    await session_mgr.persist_and_broadcast(meta.id, done_event)
