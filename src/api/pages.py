"""Server-rendered pages — single Jinja2 template for the entire UI."""

import fnmatch
import pathlib
import socket
import subprocess
from pathlib import Path
from zoneinfo import ZoneInfo

import structlog
from fastapi import APIRouter, Depends, HTTPException, Query, Request
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse
from fastapi.templating import Jinja2Templates

from src.api.deps import get_session_manager, get_thread_manager
from src.api.message_utils import build_session_bootstrap_data
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SessionStatus
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.timeouts import SUBPROCESS_GIT_VERSION_TIMEOUT

log = structlog.get_logger()

_REPO_ROOT = Path(__file__).parent.parent.parent
_PT_TZ = ZoneInfo("America/Los_Angeles")


def _get_git_version() -> str:
  """Return git short hash + commit date (e.g. 'bc6b882 · 03-24'), or '' on failure."""
  try:
    short_hash = subprocess.check_output(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=_REPO_ROOT,
        text=True,
        timeout=SUBPROCESS_GIT_VERSION_TIMEOUT,
    ).strip()
    commit_date = subprocess.check_output(
        ["git", "log", "-1", "--format=%cd", "--date=format:%m-%d"],
        cwd=_REPO_ROOT,
        text=True,
        timeout=SUBPROCESS_GIT_VERSION_TIMEOUT,
    ).strip()
    return f"{short_hash} · {commit_date}"
  except Exception:
    log.warning("git_version_failed")
    return ""


_RUNTIME_GIT_VERSION = _get_git_version()

router = APIRouter()
templates = Jinja2Templates(directory=str(Path(__file__).parent.parent.parent / "web" / "templates"))


@router.get("/api/auth/status")
async def auth_status(cfg: CharlieBotConfig = Depends(get_config)):
  """Return whether access-key authentication is enabled."""
  return JSONResponse({"auth_enabled": bool(cfg.charliebot_access_key)})


@router.get("/sessions/{session_id}/events", response_class=HTMLResponse)
async def events_viewer(
    request: Request,
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """Render the JSONL events viewer page for a session."""
  try:
    session = await session_mgr.get_session(session_id)
  except (KeyError, FileNotFoundError):
    raise HTTPException(status_code=404, detail="Session not found")
  except Exception:
    log.exception("get_session_failed", session_id=session_id)
    raise HTTPException(status_code=500, detail="Failed to load session")

  if not session:
    raise HTTPException(status_code=404, detail="Session not found")

  return templates.TemplateResponse(
      request,
      "events_viewer.html",
      context={
          "session": session,
          "session_id": session_id,
          "events_url": f"/api/sessions/{session_id}/events.jsonl",
          "hostname": socket.gethostname(),
      })


@router.get("/perfetto", response_class=HTMLResponse)
async def perfetto_viewer(
    request: Request,
    trace: list[str] = Query(default=[]),
    dir: str | None = None,
    pattern: str = "*.json",
    title: str | None = None,
):
  """Render the Perfetto trace viewer page.

  Supports single trace, multiple traces, and directory auto-discovery.
  """
  trace_urls: list[str] = [
      f"/files{url}" if url.startswith("/") and not url.startswith("/files/") else url for url in trace
  ]

  if dir:
    dir_path = Path(dir)
    if dir_path.is_dir():
      filenames = sorted(f.name for f in dir_path.iterdir() if f.is_file() and fnmatch.fnmatch(f.name, pattern))
      dir_stripped = dir.rstrip("/")
      trace_urls.extend(f"/files/{dir_stripped}/{name}" for name in filenames)
    else:
      log.warning("perfetto_dir_not_found", dir=dir)

  if not trace_urls:
    raise HTTPException(status_code=400, detail="No trace files specified. Provide 'trace' or 'dir' query params.")

  trace_names = [url.rsplit("/", 1)[-1] for url in trace_urls]

  return templates.TemplateResponse(
      request,
      "perfetto.html",
      context={
          "trace_urls": trace_urls,
          "trace_names": trace_names,
          "trace_dir": dir,
          "title": title,
      })


@router.get("/diff", response_class=HTMLResponse)
async def diff_viewer(request: Request):
  """Render the GitHub-style diff viewer page."""
  return templates.TemplateResponse(request, "diff.html", context={"hostname": socket.gethostname()})


@router.get("/", response_class=HTMLResponse)
async def index(
    request: Request,
    session: str | None = None,
    session_mgr: SessionManager = Depends(get_session_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Render the full page with only critical active-session data."""
  load_errors: list[str] = []
  try:
    sessions = await session_mgr.list_sessions(
        status=SessionStatus.ACTIVE,
        scheduled=False,
        include_running_status=True,
        include_pending_trigger_status=True,
    )
  except Exception:
    log.exception("list_sessions_failed")
    sessions = []
    load_errors.append("Failed to load sessions. Check server logs for details.")

  active_session = None
  threads = []
  triggers = []
  session_usage = None
  pending_draft: dict | None = None
  event_count = 0
  session_bootstrap: dict | None = None
  if session:
    try:
      active_session = await session_mgr.get_session(session)
    except Exception:
      log.exception("get_session_failed", session_id=session)

    if active_session:
      try:
        bootstrap = await build_session_bootstrap_data(session, session_mgr)
        active_session = bootstrap.session
        pending_draft = bootstrap.pending_draft
        event_count = bootstrap.total_event_count
        for sidebar_session in sessions:
          if sidebar_session.id == session:
            sidebar_session.has_unread = False
        active_backend = active_session.backend or (cfg.backend_options[0].id if cfg.backend_options else "claude")
        active_backend_opt = cfg.get_backend_option(active_backend)
        session_bootstrap = {
            "session": active_session.model_dump(mode="json"),
            "messages": bootstrap.messages,
            "pending_draft": bootstrap.pending_draft,
            "event_count": bootstrap.total_event_count,
            "active_backend": active_backend,
            "active_backend_type": active_backend_opt.type if active_backend_opt else "",
            "has_more": bootstrap.has_more,
        }
      except Exception:
        log.exception("load_session_data_failed", session_id=session)
        load_errors.append("Failed to load session data. Check server logs for details.")
  elif session is None and sessions:
    return RedirectResponse(f"/?session={sessions[0].id}")

  active_backend = active_session.backend if active_session else (
      cfg.backend_options[0].id if cfg.backend_options else "claude")
  active_backend_opt = cfg.get_backend_option(active_backend)
  active_backend_label = active_backend_opt.label if active_backend_opt else active_backend
  active_backend_type = active_backend_opt.type if active_backend_opt else ""

  return templates.TemplateResponse(
      request,
      "index.html",
      context={
          "sessions": sessions,
          "active_session": active_session,
          "pending_draft": pending_draft,
          "threads": threads,
          "triggers": triggers,
          "pt_tz": _PT_TZ,
          "event_count": event_count,
          "session_usage": session_usage,
          "session_bootstrap": session_bootstrap,
          "backend_options": cfg.backend_options,
          "active_backend": active_backend,
          "active_backend_label": active_backend_label,
          "active_backend_type": active_backend_type,
          "load_errors": load_errors,
          "auth_enabled": bool(cfg.charliebot_access_key),
          "hostname": socket.gethostname(),
          "user_home": str(pathlib.Path.home()),
          "version": _RUNTIME_GIT_VERSION,
      })
