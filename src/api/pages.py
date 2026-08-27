"""Server-rendered pages — single Jinja2 template for the entire UI."""

import asyncio
import concurrent.futures
import datetime as dt
import fnmatch
import gzip
import hashlib
import json
import multiprocessing
import os
import shutil
import socket
import subprocess
import tempfile
from collections.abc import Awaitable, Callable
from pathlib import Path
from urllib.parse import urlencode, urlparse
from zoneinfo import ZoneInfo

import structlog
from fastapi import APIRouter, Depends, HTTPException, Query, Request
from fastapi.responses import FileResponse, HTMLResponse, JSONResponse, RedirectResponse
from fastapi.templating import Jinja2Templates

from src.api.code_server import is_code_server_available
from src.api.deps import get_session_manager
from src.api.message_utils import build_session_bootstrap_data
from src.api.sessions import _bootstrap_payload
from src.core.config import HOUSE_TIMEZONE, CharlieBotConfig, get_config
from src.core.models import SessionStatus
from src.core.ncu_parsing import NcuParseError, parse_ncu_report
from src.core.sessions import SessionManager
from src.core.timeouts import SUBPROCESS_GIT_VERSION_TIMEOUT
from src.core.token_tally import TokenTally, collect_token_usage
from src.core.trace_merge import merge_traces

log = structlog.get_logger()

_REPO_ROOT = Path(__file__).parent.parent.parent
_PT_TZ = ZoneInfo(HOUSE_TIMEZONE)
_PERFETTO_MERGE_CACHE_LIMIT = 24
# Prefixes the file server answers on (server.py mounts one router under both). A trace= input
# names an absolute path under either of them.
_FILE_SERVER_PREFIXES = ("/files", "/absolute_filepath")

# Destinations served by this server, listed on the home page. The same on every
# host, so they live in code rather than config; each renders as a card linking
# straight to the page.
_HOME_DESTINATIONS: tuple[dict[str, str], ...] = (
    {"name": "Chat", "url": "/", "description": "The CharlieBot chat and session UI."},
    {"name": "Token usage by model", "url": "/token-usage",
     "description": "Tokens per model across every agent log on this host."},
    {"name": "Diff viewer", "url": "/diff", "description": "Browse a repository diff between two refs."},
    {"name": "File browser", "url": "/files/", "description": "Browse any file on this host's filesystem."},
)

_HOME_PROBE_TIMEOUT_S = 0.3


def _probe_home_service(url: str) -> bool:
  """TCP-connect to the host and port parsed out of *url*; True when the connect succeeds.

  The port defaults by scheme (443 for https, 80 for http). Any failure — refused or
  timed-out connect, an unparseable or out-of-range port, or a URL with no host —
  returns False; a probe never raises out of the home route.
  """
  try:
    parsed = urlparse(url)
    host = parsed.hostname
    port = parsed.port
  except ValueError:
    return False
  if not host:
    return False
  if port is None:
    port = 443 if parsed.scheme == "https" else 80
  try:
    with socket.create_connection((host, port), timeout=_HOME_PROBE_TIMEOUT_S):
      return True
  except OSError:
    return False

# Single-flight holder for the current in-flight token-usage collection. Concurrent requests
# await the same task and share one scan; it is cleared on completion so the next request scans
# afresh rather than re-servicing a stale snapshot.
_token_usage_task: asyncio.Task | None = None

# Single-flight registry for in-flight Perfetto cache builds, keyed by cache key. Concurrent
# requests for the same key share one build; the task removes itself on completion, success or
# failure, so a later request retries rather than inheriting a stale failure.
_merge_tasks: dict[str, asyncio.Task] = {}
# Bounded process pool for the CPU-bound merge body, created lazily on first use and shut down
# from the server lifespan's shutdown half.
_merge_executor_instance: concurrent.futures.ProcessPoolExecutor | None = None
_MERGE_POOL_WORKERS = 2


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
  except (KeyError, FileNotFoundError) as e:
    raise HTTPException(status_code=404, detail="Session not found") from e
  except Exception as e:
    log.exception("get_session_failed", session_id=session_id)
    raise HTTPException(status_code=500, detail="Failed to load session") from e

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
    slim: bool | None = None,
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
  is_merge = trace_url.startswith("/perfetto/merged") and (len(inputs) > 1 or bool(slim))

  return templates.TemplateResponse(
      request,
      "perfetto.html",
      context={
          "trace_url": trace_url,
          "title": display_title,
          "warn": warn,
          "merge_count": len(inputs),
          "is_merge": is_merge,
          "is_direct_pass": trace_url.startswith("/perfetto/merged") and not is_merge,
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
  for prefix in _FILE_SERVER_PREFIXES:
    if value.startswith(f"{prefix}/"):
      return value, Path(value.removeprefix(prefix))
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


def _merge_executor() -> concurrent.futures.ProcessPoolExecutor | None:
  """Return the shared merge process pool, building it on first use."""
  global _merge_executor_instance
  if _merge_executor_instance is None:
    _merge_executor_instance = concurrent.futures.ProcessPoolExecutor(
        max_workers=_MERGE_POOL_WORKERS, mp_context=multiprocessing.get_context("spawn"))
  return _merge_executor_instance


def shutdown_merge_executor() -> None:
  """Shut down the shared merge process pool. Called from the server lifespan."""
  global _merge_executor_instance
  instance = _merge_executor_instance
  _merge_executor_instance = None
  if instance is not None:
    instance.shutdown(wait=False, cancel_futures=True)


async def _await_shared_merge(cache_key: str, build_fn: Callable[[], Awaitable[Path]]) -> Path:
  """Return the cached product for *cache_key*, sharing any in-flight build for it.

  The shared build is awaited through ``asyncio.shield`` so a client that disconnects
  mid-build cancels only its own wait, never the shared task; the abandoned build
  completes and lands in the cache (cache-warming). A failed build propagates to every
  waiter, and the registry entry is removed so a later request retries.
  """
  existing = _merge_tasks.get(cache_key)
  if existing is None:
    existing = _merge_tasks[cache_key] = asyncio.create_task(build_fn())
    existing.add_done_callback(lambda _task: _merge_tasks.pop(cache_key, None))
  return await asyncio.shield(existing)


async def _cached_gzip_build(cache_key: str, build: Callable[[Path], Awaitable[None]]) -> Path:
  """Serve the cached ``<cache_key>.json.gz``; on a miss, run *build* and cache its product.

  The build writes a temp file that ``os.replace`` moves into place only on success —
  a failed or interrupted build must leave no partial cache entry. The temp file sits
  in the cache dir itself because ``os.replace`` cannot cross filesystems.
  """
  cache_dir = _perfetto_merge_cache_dir()
  cache_dir.mkdir(parents=True, exist_ok=True)
  cache_path = cache_dir / f"{cache_key}.json.gz"
  if cache_path.is_file():
    os.utime(cache_path, None)
    return cache_path

  async def run_build() -> Path:
    descriptor, temp_name = tempfile.mkstemp(dir=cache_dir, suffix=".tmp")
    os.close(descriptor)
    temp_path = Path(temp_name)
    try:
      await build(temp_path)
    except Exception:
      temp_path.unlink()
      raise
    os.replace(temp_path, cache_path)
    _prune_perfetto_merge_cache(cache_path)
    return cache_path

  return await _await_shared_merge(cache_key, run_build)


async def _cached_merge(paths: list[Path], slim: bool) -> Path:

  async def build(temp_path: Path) -> None:
    await asyncio.get_running_loop().run_in_executor(_merge_executor(), merge_traces, paths, temp_path, slim)

  return await _cached_gzip_build(_merge_cache_key(paths, slim, "merge"), build)


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


async def _cached_direct_pass(path: Path) -> Path:

  async def build(temp_path: Path) -> None:
    await asyncio.get_running_loop().run_in_executor(None, _build_direct_pass_gzip, path, temp_path)

  return await _cached_gzip_build(_merge_cache_key([path], False, "gzip"), build)


@router.get("/perfetto/merged")
async def perfetto_merged(
    trace: list[str] = Query(default=[]),
    dir: str | None = None,
    pattern: str = "*.json",
    slim: bool = False,
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
      cache_path = await _cached_direct_pass(resolved_paths[0])
    else:
      cache_path = await _cached_merge(resolved_paths, bool(slim))
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


def _compact(n: float) -> str:
  """Render a large count compactly: 1.23M, 456K, else a plain comma-formatted number."""
  for cut, suffix in ((1e9, "B"), (1e6, "M"), (1e3, "K")):
    if abs(n) >= cut:
      return f"{n / cut:.2f}".rstrip("0").rstrip(".") + suffix
  return f"{int(n):,}"


def _token_usage_context(tally: TokenTally) -> dict:
  """Prepare the display context for the token_usage template from one tally.

  Computes the aggregate stats the page renders server-side (hero, tiles, conclusions),
  the compact number strings, and the serialized JS payload for the charts and table.
  """
  rows = tally.rows
  tot = {
      "in_fresh": sum(r.in_fresh for r in rows),
      "cache_write": sum(r.cache_write for r in rows),
      "cache_read": sum(r.cache_read for r in rows),
      "output": sum(r.output for r in rows),
      "total": sum(r.total for r in rows),
      "calls": sum(r.calls for r in rows),
  }
  window = (
      (min(r.first for r in rows if r.first), max(r.last for r in rows if r.last))
      if rows and any(r.first for r in rows)
      else ("", "")
  )
  cache_share = tot["cache_read"] / tot["total"] * 100 if tot["total"] else 0.0
  out_share = tot["output"] / tot["total"] if tot["total"] else 0.0
  top = max(rows, key=lambda r: r.total) if rows else None
  top_out = max(rows, key=lambda r: r.output) if rows else None
  per_src: dict[str, dict] = {}
  for src in ("Claude Code", "Codex", "opencode"):
    sub = [r for r in rows if r.source == src]
    per_src[src] = {
        "t_comp": _compact(sum(r.total for r in sub)),
        "output": sum(r.output for r in sub),
        "models": len(sub),
        "share": sum(r.total for r in sub) / tot["total"] * 100 if tot["total"] else 0.0,
    }
  payload = {
      "rows": [
          {
              "model": r.model, "source": r.source, "total": r.total, "output": r.output,
              "in_fresh": r.in_fresh, "cache_write": r.cache_write, "cache_read": r.cache_read,
              "calls": r.calls,
              "accounts": [
                  {"name": a.name, "calls": a.calls, "output": a.output, "total": a.total}
                  for a in r.accounts
              ],
              "slot": {"Claude Code": 1, "Codex": 2, "opencode": 3}[r.source],
              "window": f"{r.first} → {r.last}",
          }
          for r in rows
      ],
  }
  payload = json.dumps(payload, ensure_ascii=False)
  ctx = {
      "rows": rows,
      "tot_compact": _compact(tot["total"]),
      "in_compact": _compact(tot["in_fresh"] + tot["cache_write"] + tot["cache_read"]),
      "out_compact": _compact(tot["output"]),
      "cr_compact": _compact(tot["cache_read"]),
      "cw_compact": _compact(tot["cache_write"]),
      "fresh_compact": _compact(tot["in_fresh"]),
      "fresh_percent": tot["in_fresh"] / tot["total"] * 100 if tot["total"] else 0.0,
      "out_share": out_share,
      "per_src": per_src,
      "tot_calls": f"{tot['calls']:,}",
      "top_escaped": top.model if top else "",
      "top_compact": _compact(top.total) if top else "0",
      "top_out_escaped": top_out.model if top_out else "",
      "top_out_compact": _compact(top_out.output) if top_out else "0",
      "elapsed_s": tally.elapsed_s,
      "scanned_compact": _compact(tally.scanned_bytes),
  }
  return {
      "ctx": ctx,
      "payload": payload,
      "window": window,
      "window_str": f"{window[0]} → {window[1]}" if rows else "",
      "cache_share": cache_share,
      "generated": dt.datetime.now().astimezone().strftime("%Y-%m-%d %H:%M %Z"),
      "notes": tally.notes,
  }


@router.get("/token-usage", response_class=HTMLResponse)
async def token_usage_viewer(request: Request):
  """Render the per-model token usage tally page.

  Runs the collection in a thread pool (never on the event loop) and, when a collection is
  already in flight, awaits and shares it instead of starting a second scan.
  """
  global _token_usage_task
  task = _token_usage_task
  if task is None:
    task = _token_usage_task = asyncio.create_task(asyncio.to_thread(collect_token_usage))
  tally = await task
  if _token_usage_task is task:
    # Only the last joiner to observe its own task still installed clears it; a joiner that
    # resumes after a newer task has already replaced it must not clobber that newer task.
    _token_usage_task = None
  return templates.TemplateResponse(
      request,
      "token_usage.html",
      context=_token_usage_context(tally),
  )


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


@router.get("/home", response_class=HTMLResponse)
async def home_page(request: Request, cfg: CharlieBotConfig = Depends(get_config)):
  """Render the home page: this server's destinations plus the per-host external services.

  Each external service is probed by TCP-connecting to the host and port parsed out of
  its configured ``url`` — the same address its card links to. Probes run off the event
  loop, concurrently, and nothing is persisted or cached.
  """
  statuses = await asyncio.gather(
      *(asyncio.to_thread(_probe_home_service, service.url) for service in cfg.home_services))
  services = [
      {
          "name": service.name,
          "description": service.description,
          "url": service.url,
          "status": "up" if up else "down",
      }
      for service, up in zip(cfg.home_services, statuses)
  ]
  return templates.TemplateResponse(
      request,
      "home.html",
      context={
          "hostname": socket.gethostname(),
          "destinations": _HOME_DESTINATIONS,
          "services": services,
      })


@router.get("/", response_class=HTMLResponse)
async def index(
    request: Request,
    session: str | None = None,
    session_mgr: SessionManager = Depends(get_session_manager),
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
        session_bootstrap = _bootstrap_payload(bootstrap, cfg)
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
