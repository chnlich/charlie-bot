"""Chat API routes — triggers master CC process, returns 202 Accepted."""

import pathlib

import aiofiles
import fastapi
from fastapi import responses

from src.infra import config, log_once, models
from src.infra import event_types as ET
from src.runtime import message_events, run_token, runs, session_dispatch, session_events, task_errors, task_sessions
from src.runtime.api import deps, sessions

log = log_once.LazyStructlogLogger()

router = fastapi.APIRouter()


@router.post("/{session_id}/upload")
async def upload_file(
    session_id: str,
    file: fastapi.UploadFile = fastapi.File(...),
    _meta: models.SessionMetadata = fastapi.Depends(deps.require_session),
    cfg: config.CharlieBotConfig = fastapi.Depends(deps.get_config_on_loop),
) -> dict:
  """Upload a file to the session's uploads directory. Returns {filename, path, size}."""
  uploads_dir = cfg.sessions_dir / session_id / "uploads"
  uploads_dir.mkdir(parents=True, exist_ok=True)

  dest = uploads_dir / pathlib.Path(file.filename or "upload").name
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
    raise fastapi.HTTPException(status_code=500, detail="Failed to save uploaded file") from e

  log.info("file_uploaded", session=session_id, filename=file.filename, size=size)
  return {"filename": file.filename, "path": str(dest.resolve()), "size": size}


@router.post("/{session_id}/message")
async def send_message(
    session_id: str,
    req: models.SendMessageRequest,
    _meta: models.SessionMetadata = fastapi.Depends(deps.require_session),
    task_mgr: task_sessions.TaskTreeManager = fastapi.Depends(deps.get_task_manager),
    caller: run_token.CallerIdentity = fastapi.Depends(deps.require_caller),
) -> responses.JSONResponse:
  """Send a message to the master CC agent. Returns 202; response streams via WebSocket."""
  uploaded_files = message_events.serialize_uploaded_files(req.uploaded_files)
  event_type = session_dispatch.input_event_type_for_caller(caller)
  from_session, from_session_name = session_dispatch.agent_provenance(caller) if event_type != ET.USER else (None, None)
  try:
    admitted = await task_mgr.dispatch.admit_input(
        session_id,
        event_type=event_type,
        content=req.content,
        actor="user" if event_type == ET.USER else "agent",
        uploaded_files=uploaded_files,
        from_session=from_session,
        from_session_name=from_session_name,
        input_mode=req.input_mode,
    )
    decision = await task_mgr.dispatch.dispatch_pending(session_id)
  except (task_errors.TaskConflictError, task_errors.TaskForbiddenError, task_errors.TaskInvalidError) as e:
    raise sessions._task_http_error(e) from e
  log.info(
      "send_message",
      session=session_id,
      content_chars=len(req.content),
      uploaded_files_count=len(uploaded_files),
  )
  return responses.JSONResponse(
      status_code=202,
      content={
          "status": "accepted",
          "input_event_id": str(admitted.get("id")),
          "launch": bool(decision.get("launch")),
          **({
              "reason": decision["reason"]
          } if decision.get("reason") else {}),
      })


async def _cancel_task_node_runs(session_id: str, task_mgr: task_sessions.TaskTreeManager) -> int:
  """Stop every launched, non-terminal Run of one task-tree node.

  Each stop rides the run store's own entry point: the durable request lands
  first, then the identity-checked signal and exit observation, so a run
  re-attached after a server restart stops the same way. The request id is
  derived from the run id, so a repeated press is the same idempotent request.
  Returns how many stops were requested.
  """
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
    events: session_events.SessionEvents = fastapi.Depends(deps.get_session_events),
    task_mgr: task_sessions.TaskTreeManager = fastapi.Depends(deps.get_task_manager),
    _meta: models.SessionMetadata = fastapi.Depends(deps.require_session),
) -> dict:
  """Stop every launched, non-terminal Run of the task."""
  try:
    requested = await _cancel_task_node_runs(session_id, task_mgr)
  except runs.RunIdentityConflictError as e:
    raise sessions._task_http_error(e) from e
  if requested == 0:
    await events.persist_and_broadcast(
        session_id, {
            "type": ET.ASSISTANT_ERROR,
            "content": "No active master agent to cancel.",
        })
    raise fastapi.HTTPException(status_code=404, detail="No active master agent")
  return {"ok": True}
