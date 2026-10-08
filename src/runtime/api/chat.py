"""Chat API routes — triggers master CC process, returns 202 Accepted."""

from pathlib import Path
import aiofiles
from fastapi import APIRouter, Depends, File, HTTPException, UploadFile
from fastapi.responses import JSONResponse

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SendMessageRequest, SessionMetadata
from src.runtime.api.deps import (
    get_config_on_loop,
    get_session_manager,
    get_task_manager,
    require_caller,
    require_session,
)
from src.runtime.message_events import serialize_uploaded_files
from src.runtime.run_token import CallerIdentity
from src.runtime.runs import RunIdentityConflictError
from src.runtime.session_dispatch import agent_provenance, input_event_type_for_caller
from src.runtime.sessions import SessionManager
from src.runtime.task_errors import TaskConflictError, TaskForbiddenError, TaskInvalidError
from src.runtime.task_sessions import TaskTreeManager

log = LazyStructlogLogger()

router = APIRouter()


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
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> JSONResponse:
  """Send a message to the master CC agent. Returns 202; response streams via WebSocket."""
  uploaded_files = serialize_uploaded_files(req.uploaded_files)
  event_type = input_event_type_for_caller(caller)
  from_session, from_session_name = agent_provenance(caller) if event_type != ET.USER else (None, None)
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
  log.info(
      "send_message",
      session=session_id,
      content_chars=len(req.content),
      uploaded_files_count=len(uploaded_files),
  )
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


async def _cancel_task_node_runs(session_id: str, task_mgr: TaskTreeManager) -> int:
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
    session_mgr: SessionManager = Depends(get_session_manager),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    _meta: SessionMetadata = Depends(require_session),
) -> dict:
  """Stop every launched, non-terminal Run of the task."""
  try:
    requested = await _cancel_task_node_runs(session_id, task_mgr)
  except RunIdentityConflictError as e:
    from src.runtime.api.sessions import _task_http_error
    raise _task_http_error(e) from e
  if requested == 0:
    await session_mgr.persist_and_broadcast(
        session_id, {
            "type": ET.ASSISTANT_ERROR,
            "content": "No active master agent to cancel.",
        })
    raise HTTPException(status_code=404, detail="No active master agent")
  return {"ok": True}
