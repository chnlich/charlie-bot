"""Server-rendered pages — single Jinja2 template for the entire UI."""

import asyncio
import fnmatch
import gzip
import hashlib
import json
import os
import shutil
import socket
import subprocess
import tempfile
from pathlib import Path
from typing import Literal
from urllib.parse import urlencode
from zoneinfo import ZoneInfo

import structlog
from fastapi import APIRouter, Depends, HTTPException, Query, Request
from fastapi.responses import FileResponse, HTMLResponse, JSONResponse, RedirectResponse
from fastapi.templating import Jinja2Templates

from src.api.code_server import is_code_server_available
from src.api.deps import get_session_manager, get_thread_manager
from src.api.message_utils import build_session_bootstrap_data
from src.core.config import CharlieBotConfig, get_config
from src.core.models import SessionStatus
from src.core.ncu_parsing import NcuParseError, parse_ncu_report
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager
from src.core.timeouts import SUBPROCESS_GIT_VERSION_TIMEOUT
from src.core.trace_merge import merge_traces

log = structlog.get_logger()

_REPO_ROOT = Path(__file__).parent.parent.parent
_PT_TZ = ZoneInfo("America/Los_Angeles")
_PERFETTO_MERGE_CACHE_LIMIT = 8


def _perfetto_merge_cache_dir() -> Path:
  """This profile's Perfetto merge cache. Resolved per call, never at import."""
  return get_config().charliebot_home / "cache" / "perfetto_merge"


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


def _static_asset_version() -> str:
  """Cache-bust token for static assets, derived from the pinned runtime git version."""
  return _RUNTIME_GIT_VERSION.replace(" · ", "-").replace(" ", "-")

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
          "static_asset_version": _static_asset_version(),
      })


@router.get("/perfetto", response_class=HTMLResponse)
async def perfetto_viewer(
    request: Request,
    trace: list[str] = Query(default=[]),
    dir: str | None = None,
    pattern: str = "*.json",
    title: str | None = None,
    slim: Literal[0, 1] | None = None,
):
  """Render the Perfetto trace viewer page.

  Supports single trace, multiple traces, and directory auto-discovery.
  """
  inputs = [_trace_input(value) for value in trace]
  if dir is not None:
    discovered = await asyncio.to_thread(_discover_trace_paths, dir, pattern)
    inputs.extend((f"/files{path}", path) for path in discovered)

  if not inputs:
    raise HTTPException(status_code=400, detail="No trace files specified. Provide 'trace' or 'dir' query params.")

  warn = None
  if await asyncio.to_thread(_all_local_json_traces, inputs):
    query: list[tuple[str, str]] = [("trace", str(path)) for _, path in inputs[:len(trace)]]
    if dir is not None:
      query.extend((("dir", dir), ("pattern", pattern)))
    if slim is not None:
      query.append(("slim", str(slim)))
    trace_url = f"/perfetto/merged?{urlencode(query)}"
  else:
    trace_url = inputs[0][0]
    if len(inputs) > 1:
      warn = "⚠ Remote or non-JSON traces cannot be merged, showing first trace only"

  display_title = title or dir or inputs[0][0].rsplit("/", 1)[-1]

  return templates.TemplateResponse(
      request,
      "perfetto.html",
      context={
          "trace_url": trace_url,
          "title": display_title,
          "warn": warn,
      })


def _discover_trace_paths(directory: str, pattern: str) -> list[Path]:
  dir_path = Path(directory)
  if not dir_path.is_dir():
    log.warning("perfetto_dir_not_found", dir=directory)
    return []
  return sorted(
      (path for path in dir_path.iterdir() if path.is_file() and fnmatch.fnmatch(path.name, pattern)),
      key=lambda path: path.name,
  )


def _trace_input(value: str) -> tuple[str, Path | None]:
  if value.startswith("/files/"):
    return value, Path(value.removeprefix("/files"))
  if value.startswith("/"):
    return f"/files{value}", Path(value)
  return value, None


def _all_local_json_traces(inputs: list[tuple[str, Path | None]]) -> bool:
  return all(path is not None and path.is_file() and _is_json_trace(path) for _, path in inputs)


def _is_json_trace(path: Path) -> bool:
  with path.open("rb") as trace_file:
    prefix = trace_file.read(64)
  for byte in prefix:
    if byte in b" \t\n\r":
      continue
    return byte in b"{["
  return False


def _merge_cache_key(paths: list[Path], slim: bool, mode: str) -> str:
  inputs = []
  for path in paths:
    stat = path.stat()
    inputs.append((str(path), stat.st_size, stat.st_mtime_ns))
  payload = json.dumps({"paths": inputs, "slim": slim, "mode": mode}, separators=(",", ":"))
  return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def _prune_perfetto_merge_cache(fresh_path: Path) -> None:
  entries = sorted(
      (path for path in _perfetto_merge_cache_dir().glob("*.json.gz") if path != fresh_path),
      key=lambda path: path.stat().st_mtime_ns,
      reverse=True,
  )
  for stale_path in entries[_PERFETTO_MERGE_CACHE_LIMIT - 1:]:
    stale_path.unlink()


def _cached_merge(paths: list[Path], slim: bool) -> Path:
  cache_dir = _perfetto_merge_cache_dir()
  cache_dir.mkdir(parents=True, exist_ok=True)
  cache_path = cache_dir / f"{_merge_cache_key(paths, slim, 'merge')}.json.gz"
  if cache_path.is_file():
    return cache_path

  descriptor, temp_name = tempfile.mkstemp(dir=cache_dir, suffix=".tmp")
  os.close(descriptor)
  temp_path = Path(temp_name)
  try:
    merge_traces(paths, temp_path, slim)
  except Exception:
    temp_path.unlink()
    raise
  os.replace(temp_path, cache_path)
  _prune_perfetto_merge_cache(cache_path)
  return cache_path


def _build_direct_pass_gzip(path: Path, out_path: Path) -> None:
  """Validate the file is parseable JSON, then stream-compress the original bytes unchanged.

  Peaks around 2.25 GB RSS for a 525.8 MB file (same order as the merge path's per-input
  json.load) and reads the source file a second time, after validation, to compress it.
  """
  with path.open("rb") as validate_file:
    json.load(validate_file)
  with path.open("rb") as source_file, open(out_path, "wb") as raw_output:
    with gzip.GzipFile(fileobj=raw_output, mode="wb", compresslevel=6) as gzip_output:
      shutil.copyfileobj(source_file, gzip_output, length=65536)


def _cached_direct_pass(path: Path) -> Path:
  cache_dir = _perfetto_merge_cache_dir()
  cache_dir.mkdir(parents=True, exist_ok=True)
  cache_path = cache_dir / f"{_merge_cache_key([path], False, 'gzip')}.json.gz"
  if cache_path.is_file():
    return cache_path

  descriptor, temp_name = tempfile.mkstemp(dir=cache_dir, suffix=".tmp")
  os.close(descriptor)
  temp_path = Path(temp_name)
  try:
    _build_direct_pass_gzip(path, temp_path)
  except Exception:
    temp_path.unlink()
    raise
  os.replace(temp_path, cache_path)
  _prune_perfetto_merge_cache(cache_path)
  return cache_path


@router.get("/perfetto/merged")
async def perfetto_merged(
    trace: list[str] = Query(default=[]),
    dir: str | None = None,
    pattern: str = "*.json",
    slim: Literal[0, 1] = 0,
) -> FileResponse:
  """Merge local Chrome JSON traces and serve the cached gzip output."""
  if not trace and dir is None:
    raise HTTPException(status_code=400, detail="Provide at least one trace path with 'trace' or 'dir'.")

  paths = [Path(value) for value in trace]
  if dir is not None:
    discovered = await asyncio.to_thread(_discover_trace_paths, dir, pattern)
    if not discovered:
      raise HTTPException(status_code=400, detail="Provide at least one trace path; 'dir' matched no files.")
    paths.extend(discovered)
  if not paths:
    raise HTTPException(status_code=400, detail="Provide at least one trace path with 'trace' or 'dir'.")

  resolved_paths: list[Path] = []
  for path in paths:
    if not await asyncio.to_thread(path.is_file):
      raise HTTPException(status_code=404, detail=f"Trace path not found: {path}")
    resolved_path = await asyncio.to_thread(path.resolve)
    if not await asyncio.to_thread(_is_json_trace, resolved_path):
      raise HTTPException(status_code=400, detail=f"Trace is not JSON: {path}")
    resolved_paths.append(resolved_path)

  try:
    if len(resolved_paths) == 1 and not slim:
      cache_path = await asyncio.to_thread(_cached_direct_pass, resolved_paths[0])
    else:
      cache_path = await asyncio.to_thread(_cached_merge, resolved_paths, bool(slim))
  except Exception as error:
    log.exception("perfetto_merge_failed", paths=[str(path) for path in resolved_paths], slim=slim)
    raise HTTPException(status_code=500, detail=str(error)) from error
  return FileResponse(cache_path, media_type="application/gzip")


def _ncu_error_page(request: Request, message: str, status_code: int) -> HTMLResponse:
  """Render ncu.html with a clean error message and a 4xx status."""
  return templates.TemplateResponse(
      request,
      "ncu.html",
      context={
          "error": message,
          "report": None
      },
      status_code=status_code,
  )


@router.get("/ncu", response_class=HTMLResponse)
async def ncu_viewer(
    request: Request,
    file: list[str] = Query(default=[]),
):
  """Render the Nsight Compute (.ncu-rep) report viewer page.

  `file` is a repeatable list of absolute paths. v1 renders the first report;
  additional paths are accepted but only noted, not diffed.
  """
  if not file:
    return _ncu_error_page(
        request,
        "No report specified. Provide a 'file' query param with an absolute path to a .ncu-rep file.",
        400,
    )

  target = file[0]
  path = Path(target)
  if not path.is_absolute():
    return _ncu_error_page(request, f"Report path must be absolute: {target}", 400)

  if not await asyncio.to_thread(path.is_file):
    return _ncu_error_page(request, f"Report not found: {target}", 404)

  try:
    report = await asyncio.to_thread(parse_ncu_report, str(path))
  except NcuParseError as exc:
    return _ncu_error_page(request, str(exc), 422)

  download_url = "/files" + str(path)
  return templates.TemplateResponse(
      request,
      "ncu.html",
      context={
          "error": None,
          "report": report,
          "report_path": str(path),
          "filename": path.name,
          "download_url": download_url,
          "extra_count": len(file) - 1,
          "ncu_ui_cmd": f"ncu-ui {path}",
          "ncu_details_cmd": f"ncu --import {path} --page details",
          "ncu_source_cmd": f"ncu --import {path} --page source --print-source sass",
          "ncu_session_cmd": f"ncu --import {path} --page session",
          "hostname": socket.gethostname(),
      })


@router.get("/diff", response_class=HTMLResponse)
async def diff_viewer(request: Request, cfg: CharlieBotConfig = Depends(get_config)):
  """Render the GitHub-style diff viewer page."""
  return templates.TemplateResponse(
      request,
      "diff.html",
      context={
          "hostname": socket.gethostname(),
          "code_server_enabled": is_code_server_available(cfg),
          "static_asset_version": _static_asset_version(),
      })


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
            "oldest_message_ordinal": bootstrap.oldest_message_ordinal,
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
          "initial_sessions": [s.model_dump(mode="json") for s in sessions],
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
          "sessions_root": str(cfg.sessions_dir),
          "version": _RUNTIME_GIT_VERSION,
          "static_asset_version": _static_asset_version(),
      })
