"""Session management API routes."""

import asyncio
from datetime import datetime, timezone
from zoneinfo import ZoneInfo

import structlog
from croniter import croniter
from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import FileResponse
from starlette.responses import Response

from src.api.deps import get_session_manager, get_thread_manager, get_trigger_manager, require_session
from src.api.message_utils import build_session_view_data_fast, events_to_messages
from src.core.config import CharlieBotConfig, get_config, get_scheduled_tasks
from src.core.models import (
    CreateSessionRequest,
    DeleteGroupRequest,
    EloneSessionRequest,
    ForkSessionRequest,
    RateSessionRequest,
    RenameGroupRequest,
    RenameSessionRequest,
    SessionMetadata,
    SessionStatus,
    SetGroupRequest,
    ThreadMetadata,
)
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

log = structlog.get_logger()
router = APIRouter()


def _default_backend_id(cfg: CharlieBotConfig) -> str:
  return cfg.backend_options[0].id if cfg.backend_options else "claude-opus-4.6"


def _resolve_requested_backend(
    requested_backend: str | None,
    cfg: CharlieBotConfig,
    *,
    fallback_backend: str | None = None,
) -> str:
  """Resolve an optional backend override with codex-family alias support."""
  valid_backend_ids = {opt.id for opt in cfg.backend_options}
  resolved_fallback = fallback_backend or _default_backend_id(cfg)

  if requested_backend is not None and requested_backend in valid_backend_ids:
    log.info("using_requested_backend", backend=requested_backend)
    return requested_backend

  if requested_backend is not None and requested_backend.startswith("codex"):
    codex_option = next((opt for opt in cfg.backend_options if opt.type == "codex"), None)
    if codex_option:
      log.info("using_requested_backend_family_match", requested=requested_backend, backend=codex_option.id)
      return codex_option.id
    log.info(
        "using_fallback_backend",
        reason="codex_family_requested_but_no_codex_backend",
        requested=requested_backend,
        fallback=resolved_fallback,
    )
    return resolved_fallback

  reason = "backend_is_none" if requested_backend is None else "backend_not_in_valid_ids"
  log.info(
      "using_fallback_backend",
      reason=reason,
      requested=requested_backend,
      fallback=resolved_fallback,
  )
  if requested_backend is not None and requested_backend not in valid_backend_ids:
    log.warning(
        "invalid_backend_requested",
        requested=requested_backend,
        valid=list(valid_backend_ids),
        fallback=resolved_fallback,
    )
  return resolved_fallback


@router.get("/", response_model=list[SessionMetadata])
async def list_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  return await session_mgr.list_sessions(
      status=SessionStatus.ACTIVE,
      scheduled=False,
      include_running_status=True,
      include_pending_trigger_status=True,
  )


@router.post("/", response_model=SessionMetadata)
async def create_session(
    req: CreateSessionRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  backend = _resolve_requested_backend(req.backend, cfg, fallback_backend=_default_backend_id(cfg))
  log.info("creating_session", backend=backend, name=req.name)
  return await session_mgr.create_session(req, backend=backend)


@router.get("/projects")
async def list_projects():
  """Return git repos discovered from configured workspace_dirs."""
  from src.core.config import get_config
  cfg = get_config()
  return await asyncio.to_thread(cfg.discover_repos)


@router.get("/archived", response_model=list[SessionMetadata])
async def list_archived_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List archived sessions, newest first."""
  return await session_mgr.list_sessions(
      status=SessionStatus.ARCHIVED,
      include_running_status=True,
      include_pending_trigger_status=True,
  )


@router.get("/waiting", response_model=list[SessionMetadata])
async def list_waiting_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List waiting sessions, newest first."""
  return await session_mgr.list_sessions(
      status=SessionStatus.WAITING,
      include_running_status=True,
      include_pending_trigger_status=True,
  )


@router.get("/starred", response_model=list[SessionMetadata])
async def list_starred_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List starred sessions, newest first."""
  return await session_mgr.list_sessions(
      starred=True,
      include_running_status=True,
      include_pending_trigger_status=True,
  )


@router.get("/groups")
async def list_groups(session_mgr: SessionManager = Depends(get_session_manager)):
  """Return sorted distinct group names across all sessions."""
  sessions = await session_mgr.list_sessions()
  groups = sorted({s.group for s in sessions if s.group})
  return groups


@router.post("/groups/rename")
async def rename_group(req: RenameGroupRequest, session_mgr: SessionManager = Depends(get_session_manager)):
  """Rename a group across all sessions."""
  count = await session_mgr.rename_group(req.old_name, req.new_name)
  return {"updated": count}


@router.post("/groups/delete")
async def delete_group(req: DeleteGroupRequest, session_mgr: SessionManager = Depends(get_session_manager)):
  """Remove a group from all sessions (sets group to null)."""
  count = await session_mgr.delete_group(req.group)
  return {"updated": count}


@router.get("/scheduled", response_model=list[SessionMetadata])
async def list_scheduled_sessions(
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """List sessions with a scheduled task, newest first."""
  sessions = await session_mgr.list_sessions(
      scheduled=True,
      include_running_status=True,
      include_pending_trigger_status=True,
  )
  task_map = {t.name: t for t in get_scheduled_tasks()}
  for s in sessions:
    task = task_map.get(s.scheduled_task)
    if task:
      s.schedule_cron = task.cron
      s.schedule_enabled = task.enabled
      s.schedule_timezone = task.timezone
      s.schedule_project = task.project
      s.schedule_allow_failure = task.allow_failure
      tz = ZoneInfo(task.timezone)
      now = datetime.now(tz)
      s.schedule_next_run = croniter(task.cron, now).get_next(datetime).isoformat()
    else:
      s.schedule_enabled = False
  return sessions


@router.get('/status')
async def all_sessions_status(session_mgr: SessionManager = Depends(get_session_manager)):
  """Return derived sidebar state for every session shown in the sidebar."""
  sessions = await session_mgr.list_sessions()
  if not sessions:
    return {}
  await session_mgr.populate_sidebar_state(
      sessions,
      include_running_status=True,
      include_pending_trigger_status=True,
  )
  result: dict[str, dict] = {}
  for meta in sessions:
    result[meta.id] = {
        "has_unread": bool(meta.has_unread),
        "has_running_tasks": meta.has_running_tasks,
        "has_pending_trigger": meta.has_pending_trigger,
        "pending_trigger_count": meta.pending_trigger_count,
        "next_trigger_at": meta.next_trigger_at.isoformat() if meta.next_trigger_at else None,
    }
  return result


@router.get('/search', response_model=list[SessionMetadata])
async def search_sessions(q: str = '', session_mgr: SessionManager = Depends(get_session_manager)):
  """Full-text search across session names and chat content."""
  if not q.strip():
    return await session_mgr.list_sessions(
        status=SessionStatus.ACTIVE,
        include_running_status=True,
        include_pending_trigger_status=True,
    )
  return await session_mgr.search_sessions(
      q.strip(),
      include_running_status=True,
      include_pending_trigger_status=True,
  )


@router.get('/{session_id}/view')
async def get_session_view(
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Return data needed to render a session chat panel (SPA switch).

  Uses tail-loading: only the last 200 events are parsed and returned.
  The response includes has_more so the frontend can paginate backwards.
  """
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  await session_mgr.populate_sidebar_state(
      [meta],
      include_running_status=True,
      include_pending_trigger_status=True,
  )
  view = await build_session_view_data_fast(session_id, session_mgr, thread_mgr)
  trigger_mgr = get_trigger_manager()
  triggers = await trigger_mgr.list_triggers(session_id)
  active_backend = meta.backend or (cfg.backend_options[0].id if cfg.backend_options else "claude-opus-4.6")
  return {
      "session": meta.model_dump(mode="json"),
      "messages": view.messages,
      "threads": [t.model_dump(mode="json") for t in view.threads],
      "triggers": [tr.model_dump(mode="json") for tr in triggers],
      "event_count": view.total_event_count,
      "usage": view.usage,
      "active_backend": active_backend,
      "has_more": view.has_more,
  }


@router.get('/{session_id}/events')
async def get_session_events_page(
    session_id: str,
    before: int,
    limit: int = 200,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """Paginate backwards through session events.

  Returns events with line indices [max(0, before-limit), before) and a
  has_more flag indicating whether earlier events exist.
  """
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  start = max(0, before - limit)
  events, has_more = await asyncio.to_thread(session_mgr.load_chat_events_range, session_id, start, before)
  messages = events_to_messages(events, event_index_offset=start)
  return {"messages": messages, "has_more": has_more}


@router.post('/{session_id}/fork', response_model=SessionMetadata)
async def fork_session(
    session_id: str,
    body: ForkSessionRequest | None = None,
    parent: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Clone a session. Optional body supports event_index and backend override."""
  backend = _resolve_requested_backend(
      body.backend if body else None,
      cfg,
      fallback_backend=parent.backend,
  )
  meta = await session_mgr.fork_session(
      session_id,
      event_index=body.event_index if body else None,
      backend=backend,
  )
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  # Build bootstrap prompt so the clone reads its own events and orients itself
  clone_events_path = session_mgr.get_chat_events_path(meta.id)
  bootstrap_prompt = (
      "This session was cloned from a previous conversation.\n\n"
      f"Your events file: {clone_events_path}\n\n"
      "Please:\n"
      "1. Read the events file\n"
      "2. Focus on the last 2-3 exchanges to understand the current context\n"
      "3. Summarize the context briefly\n"
      "4. Wait for the user to tell you what to do next")

  from src.api.chat import run_and_finalize
  from src.core.tasks import create_logged_task
  create_logged_task(run_and_finalize(cfg, meta, bootstrap_prompt, session_mgr, skip_user_event=False))

  return meta


@router.post('/{session_id}/elone', response_model=SessionMetadata)
async def elone_session(
    session_id: str,
    body: EloneSessionRequest,
    parent: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Create an Elon-e session: fresh start with a bootstrap prompt that reads the parent."""
  backend = _resolve_requested_backend(body.backend, cfg, fallback_backend=parent.backend)
  meta = await session_mgr.elone_session(session_id, body.event_index, backend=backend)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")

  # Build bootstrap prompt
  parent_events_path = session_mgr.get_chat_events_path(session_id)
  bootstrap_prompt = (
      "You're taking over a task from a previous session where user wasn't satisfied.\n"
      "Usually the dissatisfaction is with the last assistant response before the takeover point.\n\n"
      f"Parent session: {session_id}\n"
      f"Events file: {parent_events_path} (read up to line {body.event_index})\n\n"
      "Your mission:\n"
      "1. Read the parent conversation\n"
      "2. Identify what the user was trying to achieve\n"
      "3. Analyze what went wrong with the previous approach\n"
      "4. State your critique concisely\n"
      "5. Propose a better approach and ask the user to confirm before proceeding")

  # Write synthetic user event and auto-start the assistant
  from src.api.chat import run_and_finalize
  from src.core.tasks import create_logged_task
  create_logged_task(run_and_finalize(cfg, meta, bootstrap_prompt, session_mgr, skip_user_event=False))

  return meta


@router.get("/{session_id}", response_model=SessionMetadata)
async def get_session(meta: SessionMetadata = Depends(require_session)):
  return meta


@router.delete("/{session_id}", response_model=SessionMetadata)
async def archive_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.archive_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.delete("/{session_id}/permanent", status_code=204)
async def delete_session_permanently(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  result = await session_mgr.delete_session_permanently(session_id)
  if not result:
    raise HTTPException(status_code=404, detail="Session not found")
  return Response(status_code=204)


@router.post("/{session_id}/unarchive", response_model=SessionMetadata)
async def unarchive_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  if meta.status != SessionStatus.ARCHIVED:
    raise HTTPException(status_code=409, detail="Session is not archived")
  return await session_mgr.unarchive_session(session_id)


@router.post("/{session_id}/star", response_model=SessionMetadata)
async def star_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.star_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.post("/{session_id}/unstar", response_model=SessionMetadata)
async def unstar_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.unstar_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.post("/{session_id}/rate", response_model=SessionMetadata)
async def rate_session(
    session_id: str,
    req: RateSessionRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  meta = await session_mgr.get_session(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  if meta.status != SessionStatus.ARCHIVED:
    raise HTTPException(status_code=409, detail="Session is not archived")
  meta.rating = req.rating
  meta.updated_at = datetime.now(timezone.utc)
  await session_mgr.save_metadata(meta)
  log.info("session_rated", session_id=session_id, rating=req.rating)
  return meta


@router.post("/{session_id}/wait", response_model=SessionMetadata)
async def wait_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  if meta.status != SessionStatus.ACTIVE:
    raise HTTPException(status_code=409, detail="Session is not active")
  return await session_mgr.mark_waiting(session_id)


@router.post("/{session_id}/unwait", response_model=SessionMetadata)
async def unwait_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  if meta.status != SessionStatus.WAITING:
    raise HTTPException(status_code=409, detail="Session is not waiting")
  return await session_mgr.unmark_waiting(session_id)


@router.patch("/{session_id}", response_model=SessionMetadata)
async def rename_session(
    session_id: str,
    req: RenameSessionRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  meta = await session_mgr.rename_session(session_id, req.name)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.post("/{session_id}/group", response_model=SessionMetadata)
async def set_session_group(
    session_id: str,
    req: SetGroupRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  meta = await session_mgr.set_group(session_id, req.group)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return meta


@router.post("/{session_id}/read")
async def mark_session_read(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  meta = await session_mgr.mark_read(session_id)
  if not meta:
    raise HTTPException(status_code=404, detail="Session not found")
  return {"ok": True}


@router.get("/{session_id}/events.jsonl")
async def get_events_jsonl(session_id: str):
  """Serve the raw chat_events.jsonl file for a session."""
  from src.core.config import get_config
  cfg = get_config()
  path = cfg.sessions_dir / session_id / "data" / "chat_events.jsonl"
  if not path.exists():
    raise HTTPException(status_code=404, detail="Events file not found")
  return FileResponse(path, media_type="application/x-ndjson")


@router.get("/{session_id}/threads", response_model=list[ThreadMetadata])
async def list_threads(session_id: str, thread_mgr: ThreadManager = Depends(get_thread_manager)):
  return await thread_mgr.list_threads(session_id)
