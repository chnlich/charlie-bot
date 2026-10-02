"""Server-rendered pages — one Jinja2 template per page under web/templates/: the chat
UI, home, diff, events viewer, token usage, NCU, and Perfetto."""

import asyncio
import concurrent.futures
import datetime as dt
import fnmatch
import hashlib
import json
import os
import re
import socket
import subprocess
import tempfile
import threading
import time
import types
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import TYPE_CHECKING
from urllib.parse import urlencode, urlparse

from fastapi import APIRouter, Depends, HTTPException, Query, Request
from fastapi.responses import FileResponse, HTMLResponse, JSONResponse, RedirectResponse
from starlette.responses import Response

from src.api.code_server import is_code_server_available
from src.api.deps import (
    SESSION_NOT_FOUND_DETAIL,
    get_config_on_loop,
    get_session_manager,
    get_task_manager,
    get_thread_manager,
)
from src.api.message_utils import build_session_bootstrap_data
from src.api.sessions import (
    _bootstrap_payload,
    _default_backend_id,
    apply_row_schedule,
    project_worker_threads,
    row_schedule_fields,
)
from src.core import direct_pass_child, trace_merge_child
from src.core.buildinfo import read_repo_head_sha
from src.core.config import CharlieBotConfig, configured_access_key, get_config
from src.core.constants import (
    AUTH_STATUS_PATH,
    FILE_SERVER_MOUNTS,
    NCU_VIEWER_PATH,
    PERFETTO_MERGED_PATH,
    PERFETTO_VIEWER_PATH,
    REPO_ROOT,
    USAGE_SOURCE_CHARLIE_BOT,
    USAGE_SOURCE_CHARLIE_CODE,
    USAGE_SOURCE_CLAUDE_CODE,
    USAGE_SOURCE_CODEX,
    USAGE_SOURCE_OPENCODE,
)
from src.core.log_once import LazyStructlogLogger
from src.core.memo import StatSignatureMemo
from src.core.models import SessionStatus
from src.core.sessions import SessionManager
from src.core.task_sessions import TaskTreeManager
from src.core.threads import ThreadManager
from src.core.timeouts import HOME_SERVICE_PROBE_TIMEOUT, SUBPROCESS_GIT_VERSION_TIMEOUT

if TYPE_CHECKING:
  from fastapi.templating import Jinja2Templates

  from src.core.usage_ledger import LedgerRow

log = LazyStructlogLogger()

_PERFETTO_MERGE_CACHE_LIMIT = 24

# Destinations served by this server, listed on the home page. The same on every
# host, so they live in code rather than config; each renders as a card linking
# straight to the page.
_HOME_DESTINATIONS: tuple[dict[str, str], ...] = (
    {
        "name": "Chat",
        "url": "/",
        "description": "The CharlieBot chat and session UI."
    },
    {
        "name": "Token usage by model",
        "url": "/token-usage",
        "description": "Tokens per model across every agent log on this host."
    },
    {
        "name": "Host login authorization",
        "url": "/host-auth",
        "description": "Per-host ssh login state and the estimated Okta renewal deadline."
    },
    {
        "name": "Diff viewer",
        "url": "/diff",
        "description": "Browse a repository diff between two refs."
    },
    {
        "name": "File browser",
        "url": f"{FILE_SERVER_MOUNTS[0]}/",
        "description": "Browse any file on this host's filesystem."
    },
)


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
    with socket.create_connection((host, port), timeout=HOME_SERVICE_PROBE_TIMEOUT):
      return True
  except OSError:
    return False


# Single-flight holder for the current in-flight token-usage capture+read. Concurrent requests
# await the same task and share one capture; it is cleared on completion so the next request
# captures afresh rather than re-servicing a stale snapshot.
_token_usage_task: asyncio.Task | None = None

# Single-flight registry for in-flight Perfetto cache builds, keyed by cache key. Concurrent
# requests for the same key share one build; the task removes itself on completion, success or
# failure, so a later request retries rather than inheriting a stale failure.
_merge_tasks: dict[str, asyncio.Task] = {}
# Bounded process pool for the CPU-bound merge body, created lazily on first use and shut down
# from the server lifespan's shutdown half. Sized to the CPUs: a multi-trace merge runs one
# member per trace on this pool and the wall is parse-bound, so more workers than the CPUs
# only add contention. The single-trace build does not ride this pool: it runs in its own
# lean child (_build_single_trace_merge), a fresh address space without the spawn worker's
# per-build re-import of this module's __main__.
# PEP 649 defers this annotation's evaluation to first introspection, so the bare name does
# not import concurrent.futures at module load. Its module __getattr__ imports .process on
# first read (multiprocessing rides it), and the M99 server import floor carries no spawn-pool
# stack for a pool that may never build.
_merge_executor_instance: concurrent.futures.ProcessPoolExecutor | None = None
_MERGE_POOL_WORKERS = min(4, os.cpu_count() or 2)

# The single-trace build children are the same big-memory work the merge pool's
# max_workers capped, so they wait on the same bound; without it a burst of
# distinct merged-view requests spawns unbounded parses against the session's
# memory-capped cgroup.
_merge_build_gate = threading.BoundedSemaphore(_MERGE_POOL_WORKERS)


def _perfetto_merge_cache_dir() -> Path:
  """This profile's Perfetto merge cache. Resolved per call, never at import."""
  return get_config().charliebot_home / "cache" / "perfetto_merge"


def _get_git_version() -> str:
  """Return git short hash + commit date (e.g. 'bc6b882 · 03-24'), or '' on failure."""
  short_hash = read_repo_head_sha(SUBPROCESS_GIT_VERSION_TIMEOUT)
  if short_hash is None:
    log.warning("git_version_failed")
    return ""
  try:
    commit_date = subprocess.check_output(
        ["git", "log", "-1", "--format=%cd", "--date=format:%m-%d"],
        cwd=REPO_ROOT,
        text=True,
        timeout=SUBPROCESS_GIT_VERSION_TIMEOUT,
    ).strip()
  except (OSError, subprocess.SubprocessError):
    log.warning("git_version_failed")
    return ""
  return f"{short_hash} · {commit_date}"


# The two git subprocesses behind the version run only when a page renders the
# token or the footer; the server import floor (docs/perf_baseline.md M99)
# depends on them staying off it.
_GIT_VERSION: str | None = None


def _git_version() -> str:
  """The module's git version, computed on first use and memoized."""
  global _GIT_VERSION
  if _GIT_VERSION is None:
    _GIT_VERSION = _get_git_version()
  return _GIT_VERSION


# Content half of the ?v= asset token, keyed on the walk-instant signature
# tuple: a file that moves after the walk keys the older digest and the next
# render's walk re-hashes it, the same pre-read rule the file memos keep.
# ``digests`` carries each file's own sha1 so a change re-reads only the moved
# files; ``digest`` is the combined hex the token appends.
_ASSET_DIGEST_STATE: dict[str, tuple | dict[str, bytes] | str] = {
    "sig": (),
    "digests": {},
    "digest": "",
}

# Per-directory record of the digest walk, keyed on the directory's own
# (mtime_ns, size): dir path -> (subdirectory names, file names). The
# per-render freshness contract stays with the per-file stat every walk takes
# — a content edit moves the file's own signature, not its directory's — so
# only the directory record (entry create, delete, rename, all of which move
# the directory stat) rides the memo; a moved directory re-scandirs on the
# next walk. Served listings are shared across renders, the no-defensive-copy
# idiom of the sibling memos.
_DIR_LISTING_MEMO_LIMIT = 64
_DIR_LISTINGS: StatSignatureMemo[str, tuple[list[str], list[str]]] = StatSignatureMemo(_DIR_LISTING_MEMO_LIMIT)


def _asset_tree_digest() -> str:
  """Content digest over the served static tree, refreshed per call.

  The ?v= token names the bytes the URL serves, and a working-tree edit can
  land between restarts, so the digest walks the tree (one stat pass over the
  static files) and re-hashes only files whose (mtime_ns, size) signature
  moved; an unchanged walk serves the memoized digest. The per-directory
  entry record rides ``_DIR_LISTINGS``, so a steady render stats each file
  and each directory once and re-scandirs only a directory whose own stat
  moved.
  """
  static_root = REPO_ROOT / "web" / "static"
  if not static_root.is_dir():
    return ""
  pairs: list[tuple[str, int, int]] = []

  # String paths only: the per-file stat rides 48 os.stat(str) calls per
  # render, and a Path division per entry would price the walk past the
  # shape it replaces.
  def walk(dir_path: str, prefix: str) -> None:
    dir_st = os.stat(dir_path)
    cached = _DIR_LISTINGS.fresh(dir_path, dir_st)
    if cached is None:
      subdirs: list[str] = []
      files: list[str] = []
      with os.scandir(dir_path) as entries:
        for entry in entries:
          if entry.is_dir():
            subdirs.append(entry.name)
          elif entry.is_file():
            files.append(entry.name)
      cached = (subdirs, files)
      _DIR_LISTINGS.record(dir_path, dir_st, cached)
    subdirs, files = cached
    for name in subdirs:
      walk(os.path.join(dir_path, name), f"{prefix}{name}/")
    for name in files:
      st = os.stat(os.path.join(dir_path, name))
      pairs.append((f"{prefix}{name}", st.st_mtime_ns, st.st_size))

  walk(str(static_root), "")
  pairs.sort()
  sig = tuple(pairs)
  if _ASSET_DIGEST_STATE["sig"] == sig:
    return str(_ASSET_DIGEST_STATE["digest"])
  old_stats = {rel: (mtime, size) for rel, mtime, size in _ASSET_DIGEST_STATE["sig"]}
  digests: dict[str, bytes] = dict(_ASSET_DIGEST_STATE["digests"])
  for rel, mtime, size in pairs:
    if rel in digests and old_stats.get(rel) == (mtime, size):
      continue
    digests[rel] = hashlib.sha1((static_root / rel).read_bytes()).digest()
  combined = hashlib.sha1()
  for rel, _, _ in pairs:
    combined.update(rel.encode())
    combined.update(b"\0")
    combined.update(digests[rel])
  value = combined.hexdigest()[:12]
  _ASSET_DIGEST_STATE["sig"] = sig
  _ASSET_DIGEST_STATE["digests"] = digests
  _ASSET_DIGEST_STATE["digest"] = value
  return value


def _static_asset_version() -> str:
  """Cache-bust token for static assets: the runtime git version plus the served tree's content digest."""
  git_part = _git_version().replace(" · ", "-").replace(" ", "-")
  return f"{git_part}-{_asset_tree_digest()}"


router = APIRouter()

# jinja2 + fastapi.templating ride every page render (~19 ms of the M99 server
# import floor, marginal over the already-loaded fastapi) and no import-time
# path touches a template, so the engine builds on first render.
_templates_instance: Jinja2Templates | None = None


def _templates() -> Jinja2Templates:
  """The request-time template engine, built on first use and reused after."""
  global _templates_instance
  if _templates_instance is None:
    from fastapi import templating

    _templates_instance = templating.Jinja2Templates(directory=str(REPO_ROOT / "web" / "templates"))
  return _templates_instance


def _trace_merge() -> types.ModuleType:
  """The request-time trace-merge stack, imported on first use and reused after.

  The merge builders and the direct-pass validator are the only consumers; the
  M99 server import floor carries no trace stack.
  """
  import src.core.trace_merge

  return src.core.trace_merge


@router.get(AUTH_STATUS_PATH)
async def auth_status() -> JSONResponse:
  """Return whether access-key authentication is enabled."""
  return JSONResponse({"auth_enabled": bool(configured_access_key())})


@router.get("/sessions/{session_id}/events", response_class=HTMLResponse)
async def events_viewer(
    request: Request,
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
) -> HTMLResponse:
  """Render the JSONL events viewer page for a session."""
  try:
    session = await session_mgr.get_session(session_id)
  except (KeyError, FileNotFoundError) as e:
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL) from e
  except Exception as e:
    log.exception("get_session_failed", session_id=session_id)
    raise HTTPException(status_code=500, detail="Failed to load session") from e

  if not session:
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL)

  return _templates().TemplateResponse(
      request,
      "events_viewer.html",
      context={
          "session": session,
          "session_id": session_id,
          "events_url": f"/api/sessions/{session_id}/events.jsonl",
          "hostname": socket.gethostname(),
          "static_asset_version": _static_asset_version(),
      })


@router.get(PERFETTO_VIEWER_PATH, response_class=HTMLResponse)
async def perfetto_viewer(
    request: Request,
    trace: list[str] = Query(default=[]),
    # alias keeps the URL query key 'dir': the page's own merged-trace link builds it.
    dir_path: str | None = Query(default=None, alias="dir"),
    pattern: str = "*.json",
    title: str | None = None,
    slim: bool | None = None,
) -> HTMLResponse:
  """Render the Perfetto trace viewer page.

  Supports single trace, multiple traces, and directory auto-discovery.
  """
  inputs = [_trace_input(value) for value in trace]
  if dir_path is not None:
    discovered = await asyncio.to_thread(_discover_trace_paths, dir_path, pattern)
    inputs.extend((f"{FILE_SERVER_MOUNTS[0]}{path}", path) for path in discovered)

  if not inputs:
    raise HTTPException(status_code=400, detail="No trace files specified. Provide 'trace' or 'dir' query params.")

  warn = None
  if await asyncio.to_thread(_all_local_json_traces, inputs):
    query: list[tuple[str, str]] = [("trace", str(path)) for _, path in inputs[:len(trace)]]
    if dir_path is not None:
      query.extend((("dir", dir_path), ("pattern", pattern)))
    if slim is not None:
      query.append(("slim", str(slim)))
    trace_url = f"{PERFETTO_MERGED_PATH}?{urlencode(query)}"
  else:
    trace_url = inputs[0][0]
    if len(inputs) > 1:
      warn = "⚠ Remote or non-JSON traces cannot be merged, showing first trace only"

  display_title = title or dir_path or inputs[0][0].rsplit("/", 1)[-1]
  is_merge = trace_url.startswith(PERFETTO_MERGED_PATH) and (len(inputs) > 1 or bool(slim))

  return _templates().TemplateResponse(
      request,
      "perfetto.html",
      context={
          "trace_url": trace_url,
          "title": display_title,
          "warn": warn,
          "merge_count": len(inputs),
          "is_merge": is_merge,
          "is_direct_pass": trace_url.startswith(PERFETTO_MERGED_PATH) and not is_merge,
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
  for prefix in FILE_SERVER_MOUNTS:
    if value.startswith(f"{prefix}/"):
      return value, Path(value.removeprefix(prefix))
  if value.startswith("/"):
    return f"{FILE_SERVER_MOUNTS[0]}{value}", Path(value)
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
    # multiprocessing rides the pool build like croniter rides its next-run
    # resolutions: the M99 server import floor carries no spawn-pool stack.
    import multiprocessing

    # One build per worker: a build's freed arenas stay mapped in the worker's
    # address space, and the next build's allocations only sometimes reuse them —
    # back-to-back builds in one worker measured 5.1 GB retained + 6.5 GB fresh
    # against this host's 12 GiB session cgroup, an OOM kill the wave bound alone
    # cannot prevent. A fresh worker starts every build at zero.
    _merge_executor_instance = concurrent.futures.ProcessPoolExecutor(
        max_workers=_MERGE_POOL_WORKERS, mp_context=multiprocessing.get_context("spawn"), max_tasks_per_child=1)
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
    if len(paths) == 1:
      await asyncio.get_running_loop().run_in_executor(None, _build_single_trace_merge, paths, temp_path, slim)
      return
    await _build_multi_trace_merge(paths, slim, temp_path)

  return await _cached_gzip_build(_merge_cache_key(paths, slim, "merge"), build)


def _build_single_trace_merge(paths: list[Path], out_path: Path, slim: bool) -> None:
  """Build the single-trace merged artifact in its own lean child process.

  The child is a fresh address space per build — the property the merge pool's
  ``max_tasks_per_child=1`` exists for (a build's freed arenas stay mapped in a
  reused worker, the next build OOMs the cgroup) — without the spawn worker's
  per-build re-import of this process's ``__main__``, the full server module the
  M99 floor prices (~0.6 s on every first view; a forkserver child pays it too).
  The gate keeps the concurrency bound the pool's max_workers was.
  """
  argv = trace_merge_child.parent_argv(paths, out_path, slim, str(Path(__file__).resolve().parents[2]))
  with _merge_build_gate:
    proc = subprocess.Popen(argv, stderr=subprocess.PIPE)
    _, stderr = proc.communicate()
  detail = stderr.decode(errors="replace").strip()
  if proc.returncode == trace_merge_child.EXIT_OK:
    return
  if proc.returncode == trace_merge_child.EXIT_NOT_A_TRACE:
    raise _trace_merge().NotATraceError(detail)
  raise RuntimeError(f"merged-trace build child failed rc={proc.returncode}: {detail}")


async def _build_multi_trace_merge(paths: list[Path], slim: bool, out_path: Path) -> None:
  """Build the multi-trace merged artifact off the event loop: member tasks on
  the merge pool, the parent thread streaming each fragment into the single
  gzip subprocess as its member completes (``build_multi_trace_merge`` owns
  the member temp space; the caller owns artifact atomicity)."""
  await asyncio.get_running_loop().run_in_executor(
      None,
      _trace_merge().build_multi_trace_merge, paths, out_path, slim, _merge_executor())


def _build_direct_pass_gzip(path: Path, out_path: Path) -> None:
  """Validate the input is a parseable Chrome-JSON trace while a child process stream-compresses the original bytes.

  The artifact is the original bytes compressed and the parse result is discarded, so the two
  passes are independent. Both passes run in the child (``direct_pass_child.main``): the
  validating parse holds the GIL for its whole run (a concurrent gzip thread makes no
  progress), and this build runs on a server thread — the in-process parse stalled the event
  loop 2010-2014 ms on the 334.3 MB corpus, every concurrent request and WebSocket with it —
  so the parse must leave the process the way the compress already does. The child reports
  its verdict by exit class; this side re-raises the same error types the route's handler
  answered before. Validation parses with orjson, the parser the merge path's build already
  parses with, so both serve shapes share one JSON boundary: the NaN/Infinity literals stdlib
  json accepts fail the build loudly here too — a literal Perfetto cannot render must not
  reach the cache.
  """
  argv = direct_pass_child.parent_argv(path, out_path, str(Path(__file__).resolve().parents[2]))
  proc = subprocess.Popen(argv, stderr=subprocess.PIPE)
  _, stderr = proc.communicate()
  detail = stderr.decode(errors="replace").strip()
  if proc.returncode == direct_pass_child.EXIT_OK:
    return
  if proc.returncode == direct_pass_child.EXIT_NOT_A_TRACE:
    raise _trace_merge().NotATraceError(detail)
  if proc.returncode == direct_pass_child.EXIT_PARSE_FAILED:
    raise ValueError(detail)
  if proc.returncode == direct_pass_child.EXIT_IGZIP_FAILED:
    raise RuntimeError(detail)
  raise RuntimeError(f"direct-pass build child failed rc={proc.returncode}: {detail}")


async def _cached_direct_pass(path: Path) -> Path:

  async def build(temp_path: Path) -> None:
    await asyncio.get_running_loop().run_in_executor(None, _build_direct_pass_gzip, path, temp_path)

  return await _cached_gzip_build(_merge_cache_key([path], slim=False, mode="gzip"), build)


@router.get(PERFETTO_MERGED_PATH)
async def perfetto_merged(
    trace: list[str] = Query(default=[]),
    # alias keeps the URL query key 'dir': perfetto_viewer's merged-trace link builds it.
    dir_path: str | None = Query(default=None, alias="dir"),
    pattern: str = "*.json",
    slim: bool = False,
) -> FileResponse:
  """Merge local Chrome JSON traces and serve the cached gzip output."""
  if not trace and dir_path is None:
    raise HTTPException(status_code=400, detail="Provide at least one trace path with 'trace' or 'dir'.")

  paths = [Path(value) for value in trace]
  if dir_path is not None:
    discovered = await asyncio.to_thread(_discover_trace_paths, dir_path, pattern)
    if not discovered:
      raise HTTPException(status_code=400, detail="Provide at least one trace path; 'dir' matched no files.")
    paths.extend(discovered)

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
  return _templates().TemplateResponse(
      request,
      "ncu.html",
      context={
          "error": message,
          "report": None
      },
      status_code=status_code,
  )


@router.get(NCU_VIEWER_PATH, response_class=HTMLResponse)
async def ncu_viewer(
    request: Request,
    file: list[str] = Query(default=[]),
) -> Response:
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

  # The NCU report parser rides the viewer like croniter rides its next-run
  # resolutions: the M99 server import floor carries no report-parsing stack.
  from src.core.ncu_parsing import NcuParseError, parse_ncu_report

  try:
    report = await asyncio.to_thread(parse_ncu_report, str(path))
  except NcuParseError as exc:
    return _ncu_error_page(request, str(exc), 422)

  download_url = FILE_SERVER_MOUNTS[0] + str(path)
  return _templates().TemplateResponse(
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
      })


def _compact(n: float) -> str:
  """Render a large count compactly: 1.23M, 456K, else a plain comma-formatted number."""
  for cut, suffix in ((1e9, "B"), (1e6, "M"), (1e3, "K")):
    if abs(n) >= cut:
      return f"{n / cut:.2f}".rstrip("0").rstrip(".") + suffix
  return f"{int(n):,}"


# The usage panel's source display order: the per-source tiles iterate it, and each
# row's slot number sent to the charts is its position here. The four sources are the
# CLIs that ran the calls: a charlie-bot row's accounts attribute to their CLI (see
# _account_source), so its CLC usage and counted fallbacks land here, and the ledger's
# own charlie-bot spelling never reaches the page.
_USAGE_SOURCES = (USAGE_SOURCE_CLAUDE_CODE, USAGE_SOURCE_CODEX, USAGE_SOURCE_OPENCODE, USAGE_SOURCE_CHARLIE_CODE)
_USAGE_SLOT = {src: slot for slot, src in enumerate(_USAGE_SOURCES, 1)}

# The self-check notes name the log each captured source read. The tally's charlie-bot
# log is CharlieBot's own, not a CLI, so its label spells that instead of the ledger's
# source spelling.
_NOTE_SOURCE_LABELS = {USAGE_SOURCE_CHARLIE_BOT: "CharlieBot logs"}


def _backend_registry() -> dict[str, object]:
  """config.yaml's backend options by id — the registry the charlie-bot accounts'
  attribution reads. The tally's capture builds the same map per capture; a backend added
  or retired reclassifies the affected accounts on the next page load."""
  return {opt.id: opt for opt in get_config().backends.options}


def _capture_ledger_rows() -> tuple[list[LedgerRow], dict[str, str], dict[str, int], float, dict[str, object]]:
  """Capture this host's new usage into the ledger, then read the page rows from the ledger
  alone — so the numbers survive deletion of the logs they were parsed from — plus the
  backend registry the rows' charlie-bot accounts attribute against.

  Runs in a thread as the page's single-flight task body. Capture and read errors propagate
  to the awaiting request: the page fails loudly instead of rendering stale rows.
  """
  # The ledger + capture stack (sqlite3, the token_tally walkers) rides the page like
  # croniter rides its next-run resolutions: the M99 server import floor carries no
  # tally stack for a page that may never load.
  from src.core.token_tally import capture_local
  from src.core.usage_ledger import UsageLedger, default_ledger_path

  started = time.monotonic()
  with UsageLedger(default_ledger_path()) as ledger:
    written = capture_local(ledger)
    rows, native_starts = ledger.model_rows_with_native_starts()
  return rows, native_starts, written, time.monotonic() - started, _backend_registry()


_MODEL_LEAF_SUFFIX = re.compile(r"\s*\([^()]*\)$")


def _model_leaf(model: str) -> str:
  """The model name as a reader knows it: the last / segment minus a trailing ' (provider)'
  suffix, case kept — `zai-org/GLM-5.3-Flash` and `Kimi-K3 (amd-kimi-k3)` read as
  GLM-5.3-Flash and Kimi-K3."""
  return _MODEL_LEAF_SUFFIX.sub("", model.rsplit("/", 1)[-1])


def _account_source(row: LedgerRow, account: str, registry: dict) -> tuple[str, bool]:
  """(page source, fallback mark) for one ledger account.

  A row's own source names the CLI whose log its records were read from, so its accounts
  keep it. A charlie-bot row's accounts are backend ids instead, so each attributes to the
  CLI that ran the call (``backend_page_source`` on *registry*): CLC usage is native to
  CharlieBot's own logs, while every other backend's counted records are fallbacks behind
  their CLI's own log — the mark the account's sub-row carries.
  """
  if row.source != USAGE_SOURCE_CHARLIE_BOT:
    return row.source, False
  # The tally rides the page like the capture stack does (see _capture_ledger_rows):
  # imported here so the module stays off the server's import floor.
  from src.core.token_tally import backend_page_source

  source = backend_page_source(account, registry)
  return source, source != USAGE_SOURCE_CHARLIE_CODE


def _merge_ledger_rows(rows: list[LedgerRow], registry: dict) -> list[dict]:
  """Fold the ledger's per-(source, model) rows into one page row per model.

  Sources spell one model differently — opencode `zai-org/GLM-5.3-Flash`, charlie-bot
  `GLM-5.3-Flash`, opencode path models `Kimi-K3 (amd-kimi-k3)` — so rows group on the
  casefolded leaf name, and versions (`claude-fable-5` vs `claude-fable-5-1`) stay apart.
  The merged row displays the largest part's (by total) spelling, carries one segment per
  attributed source for the stacked charts, and lists one (source · account) sub-row per
  account across the parts; the segments and sub-rows sum to the row. The attributed
  source is the CLI that ran the call (see ``_account_source``), so a charlie-bot row
  splits between CLC and the fallback CLIs its accounts ran on.
  """
  groups: dict[str, list[LedgerRow]] = {}
  for row in rows:
    groups.setdefault(_model_leaf(row.model).casefold(), []).append(row)
  merged = []
  for parts in groups.values():
    accounts: dict[tuple[str, str, bool], dict[str, int]] = {}
    seg_totals: dict[str, int] = {}
    seg_outputs: dict[str, int] = {}
    for part in parts:
      for account in part.accounts:
        source, fallback = _account_source(part, account.name, registry)
        acc = accounts.setdefault((source, account.name, fallback), {"calls": 0, "output": 0, "total": 0})
        acc["calls"] += account.calls
        acc["output"] += account.output
        acc["total"] += account.total
        seg_totals[source] = seg_totals.get(source, 0) + account.total
        seg_outputs[source] = seg_outputs.get(source, 0) + account.output
    first = min((p.first for p in parts if p.first), default="")
    last = max((p.last for p in parts if p.last), default="")
    ranked = sorted(accounts.items(), key=lambda kv: (-kv[1]["total"], kv[0]))
    merged.append(
        {
            "model": _model_leaf(max(parts, key=lambda p: p.total).model),
            "calls": sum(p.calls for p in parts),
            "in_fresh": sum(p.in_fresh for p in parts),
            "cache_write": sum(p.cache_write for p in parts),
            "cache_read": sum(p.cache_read for p in parts),
            "in_unsplit": sum(p.in_unsplit for p in parts),
            "output": sum(p.output for p in parts),
            "total": sum(p.total for p in parts),
            "fallback_output": sum(p.fallback_output for p in parts),
            "accounts":
                [
                    {
                        "name": f"{source} · {name}{' (fallback)' if fallback else ''}",
                        "calls": acc["calls"],
                        "output": acc["output"],
                        "total": acc["total"]
                    } for (source, name, fallback), acc in ranked
                ],
            "segments":
                [
                    {
                        "slot": _USAGE_SLOT[source],
                        "total": seg_totals[source],
                        "output": seg_outputs[source],
                    } for source in _USAGE_SOURCES if source in seg_totals
                ],
            "window": f"{first} → {last}",
        })
  merged.sort(key=lambda m: (-m["total"], m["model"]))
  return merged


def _token_usage_context(
    rows: list[LedgerRow],
    native_starts: dict[str, str],
    written: dict[str, int],
    elapsed_s: float,
    registry: dict,
) -> dict:
  """Prepare the display context for the token_usage template from one ledger read.

  Merges the ledger's rows into one page row per model for the charts, the table and the
  top ranks, while the per-source tiles count attributed accounts (they answer how much
  each CLI ran). Computes the aggregate stats the page renders server-side (hero, tiles,
  conclusions) and the serialized JS payload for the charts and table.
  """
  tot = {
      "in_fresh": sum(r.in_fresh for r in rows),
      "cache_write": sum(r.cache_write for r in rows),
      "cache_read": sum(r.cache_read for r in rows),
      "in_unsplit": sum(r.in_unsplit for r in rows),
      "output": sum(r.output for r in rows),
      "total": sum(r.total for r in rows),
      "calls": sum(r.calls for r in rows),
  }
  merged = _merge_ledger_rows(rows, registry)
  window = (
      (min(r.first for r in rows if r.first),
       max(r.last for r in rows if r.last)) if rows and any(r.first for r in rows) else ("", ""))
  cache_share = tot["cache_read"] / tot["total"] * 100 if tot["total"] else 0.0
  out_share = tot["output"] / tot["total"] if tot["total"] else 0.0
  top = max(merged, key=lambda m: m["total"]) if merged else None
  top_out = max(merged, key=lambda m: m["output"]) if merged else None
  # The tiles count attributed accounts: a charlie-bot row's accounts attribute to the CLI
  # that ran the call, so a model's CLC usage and its counted fallbacks land under their
  # own sources, and the model count dedupes on the canonical name the merged rows key on.
  sums: dict[str, dict] = {src: {"total": 0, "output": 0, "models": set()} for src in _USAGE_SOURCES}
  for row in rows:
    canonical = _model_leaf(row.model).casefold()
    for account in row.accounts:
      source, _fallback = _account_source(row, account.name, registry)
      bucket = sums[source]
      bucket["total"] += account.total
      bucket["output"] += account.output
      bucket["models"].add(canonical)
  per_src: dict[str, dict] = {}
  for src, bucket in sums.items():
    # The ledger's charlie-bot rows are all CLC usage (CLC backend threads, manager runs,
    # CLC Runs), so CLC's native start is the charlie-bot span the ledger keeps.
    ledger_src = USAGE_SOURCE_CHARLIE_BOT if src == USAGE_SOURCE_CHARLIE_CODE else src
    per_src[src] = {
        "total": bucket["total"],
        "t_comp": _compact(bucket["total"]),
        "output": bucket["output"],
        "models": len(bucket["models"]),
        "share": bucket["total"] / tot["total"] * 100 if tot["total"] else 0.0,
        "native_start": native_starts.get(ledger_src, ""),
    }
  payload = json.dumps({"rows": merged}, ensure_ascii=False)
  ctx = {
      "rows": merged,
      "tot_compact": _compact(tot["total"]),
      "in_compact": _compact(tot["in_fresh"] + tot["cache_write"] + tot["cache_read"] + tot["in_unsplit"]),
      "out_compact": _compact(tot["output"]),
      "cr_compact": _compact(tot["cache_read"]),
      "cw_compact": _compact(tot["cache_write"]),
      "fresh_compact": _compact(tot["in_fresh"]),
      "fresh_percent": tot["in_fresh"] / tot["total"] * 100 if tot["total"] else 0.0,
      "out_share": out_share,
      "per_src": per_src,
      "usage_sources": list(_USAGE_SOURCES),
      "tot_calls": f"{tot['calls']:,}",
      "top_escaped": top["model"] if top else "",
      "top_compact": _compact(top["total"]) if top else "0",
      "top_out_escaped": top_out["model"] if top_out else "",
      "top_out_compact": _compact(top_out["output"]) if top_out else "0",
      "elapsed_s": elapsed_s,
      "captured_now": f"{sum(written.values()):,}",
  }
  return {
      "ctx": ctx,
      "payload": payload,
      "window": window,
      "window_str": f"{window[0]} → {window[1]}" if rows else "",
      "cache_share": cache_share,
      "generated": dt.datetime.now().astimezone().strftime("%Y-%m-%d %H:%M %Z"),
      "notes":
          [
              f"{_NOTE_SOURCE_LABELS.get(src, src)}: {count:,} records written this load"
              for src, count in written.items()
          ],
  }


@router.get("/token-usage", response_class=HTMLResponse)
async def token_usage_viewer(request: Request) -> HTMLResponse:
  """Render the per-model token usage tally page.

  Captures new usage into the ledger and reads the rows back from it, in a thread pool
  (never on the event loop); when a capture is already in flight, later requests await
  and share it instead of starting a second one.
  """
  global _token_usage_task
  task = _token_usage_task
  if task is None:
    task = _token_usage_task = asyncio.create_task(asyncio.to_thread(_capture_ledger_rows))
  try:
    rows, native_starts, written, elapsed_s, registry = await task
  finally:
    if _token_usage_task is task:
      # Only the last joiner to observe its own task still installed clears it; a joiner that
      # resumes after a newer task has already replaced it must not clobber that newer task.
      # The clear runs on failure too: a capture that raised must not stay installed and
      # re-raise the same stale exception at every later request until a server restart.
      _token_usage_task = None
  return _templates().TemplateResponse(
      request,
      "token_usage.html",
      context=_token_usage_context(rows, native_starts, written, elapsed_s, registry),
  )


@router.get("/diff", response_class=HTMLResponse)
async def diff_viewer(request: Request, cfg: CharlieBotConfig = Depends(get_config_on_loop)) -> HTMLResponse:
  """Render the GitHub-style diff viewer page."""
  return _templates().TemplateResponse(
      request,
      "diff.html",
      context={
          "hostname": socket.gethostname(),
          "code_server_enabled": is_code_server_available(cfg),
          "static_asset_version": _static_asset_version(),
      })


@router.get("/home", response_class=HTMLResponse)
async def home_page(request: Request, cfg: CharlieBotConfig = Depends(get_config_on_loop)) -> HTMLResponse:
  """Render the home page: this server's destinations plus the per-host external services.

  Each external service is probed by TCP-connecting to the host and port parsed out of
  its configured ``url`` — the same address its card links to. Probes run off the event
  loop, concurrently, and nothing is persisted or cached.
  """
  statuses = await asyncio.gather(
      *(asyncio.to_thread(_probe_home_service, service.url) for service in cfg.ui.home_services))
  services = [
      {
          "name": service.name,
          "description": service.description,
          "url": service.url,
          "status": "up" if up else "down",
      } for service, up in zip(cfg.ui.home_services, statuses, strict=True)
  ]
  return _templates().TemplateResponse(
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
    thread: str | None = None,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> Response:
  """Render the full page with only critical active-session data.

  ``/?session=<id>`` opens a session (a worker node's messages are its Runs'
  transcript); ``/?session=<parent>&thread=<id>`` opens one legacy worker
  thread projected into the same main-chat view, read-only.
  """
  # The M99 import floor carries no speech stack (the M99 row's rule) and no
  # preview probe; both serve only this page's context build.
  from src.agents.transcription.registry import build_transcription_backends
  from src.core.session_tree_preview import is_preview_mode
  load_errors: list[str] = []
  try:
    sessions = await session_mgr.list_sessions(
        status=SessionStatus.ACTIVE,
        scheduled=False,
        include_running_status=True,
        include_pending_trigger_status=True,
    )
    # The first-paint list shares the All endpoint's membership: cron-subtree
    # rows and the chat-thread subtree ride no listing, so a firing leaf neither
    # flattens into a top-level sidebar row, a Slack/Discord thread session
    # never paints into Workspace, and neither becomes the auto-redirect target.
    cron_subtree = await session_mgr.cron_subtree_roots()
    chat_threads = await session_mgr.chat_thread_subtree_roots()
    sessions = [s for s in sessions if s.id not in cron_subtree and s.id not in chat_threads]
  except Exception:
    log.exception("list_sessions_failed")
    sessions = []
    load_errors.append("Failed to load sessions. Check server logs for details.")

  active_session = None
  pending_draft: dict | None = None
  event_count = 0
  session_bootstrap: dict | None = None
  thread_view: dict | None = None
  thread_thinking = None
  if session:
    try:
      active_session = await session_mgr.get_session(session)
    except Exception:
      log.exception("get_session_failed", session_id=session)

    if active_session and thread:
      # The legacy thread view: the page renders the parent session's chrome
      # (sidebar highlight, status poll) over the thread's projected
      # transcript, read-only, addressed by this URL.
      thread_meta = await thread_mgr.get_thread(session, thread)
      if thread_meta is None:
        load_errors.append(f"Thread {thread} not found in session {session}.")
      else:
        thread_view = {
            "session_id": session,
            "thread_id": thread,
            "description": thread_meta.description,
            "backend": thread_meta.backend or "",
        }
    if active_session:
      try:
        bootstrap = await build_session_bootstrap_data(session, session_mgr, tree=task_mgr)
        active_session = bootstrap.session
        pending_draft = bootstrap.pending_draft
        event_count = bootstrap.total_event_count
        for sidebar_session in sessions:
          if sidebar_session.id == session:
            sidebar_session.has_unread = False
        session_bootstrap = _bootstrap_payload(bootstrap, cfg)
        if thread_view is not None:
          # The header names the thread, not the parent session; the parent
          # session metadata only addresses the view.
          from src.core import worker_transcript
          entry = await asyncio.to_thread(
              worker_transcript.load_thread_transcript, cfg, cfg.sessions_dir / session, await
              thread_mgr.get_thread(session, thread), await thread_mgr.get_events_log_path(session, thread))
          session_bootstrap = {
              **session_bootstrap,
              "session":
                  {
                      **session_bootstrap["session"], "name": thread_view["description"],
                      "profile": "worker",
                      "backend": thread_view["backend"]
                  },
              "messages":
                  [m.model_dump(mode="json") if hasattr(m, "model_dump") else m for m in entry.projection.committed],
              "pending_draft": entry.projection.pending_draft,
              "event_count": entry.projection.event_count,
              "oldest_message_ordinal": 0,
              "has_more": False,
              "thread_view": thread_view,
          }
          thread_thinking = worker_transcript.thread_thinking_since(thread_meta)
      except Exception:
        log.exception("load_session_data_failed", session_id=session)
        load_errors.append("Failed to load session data. Check server logs for details.")
  elif session is None and sessions:
    return RedirectResponse(f"/?session={sessions[0].id}")

  # The first-paint sidebar list carries the legacy worker-thread leaves too;
  # projected after the redirect check so a thread row can never become the
  # auto-redirect target. Row shape matches GET /api/sessions/: the schedule
  # join stamps every row, so the first paint shows a scheduled node's clock.
  sessions = await project_worker_threads(sessions, cfg, thread_mgr)
  schedule_fields = row_schedule_fields((s.id for s in sessions), dt.datetime.now(dt.UTC))
  initial_sessions = [apply_row_schedule(s.model_dump(mode="json"), schedule_fields[s.id]) for s in sessions]

  if thread_view is not None:
    active_backend = thread_view.get("backend") or _default_backend_id(cfg)
  else:
    active_backend = (
        (active_session.run_backend or active_session.backend) if active_session else _default_backend_id(cfg))
  active_backend_opt = cfg.get_backend_option(active_backend)
  active_backend_label = active_backend_opt.label if active_backend_opt else active_backend
  active_backend_type = active_backend_opt.type if active_backend_opt else ""

  return _templates().TemplateResponse(
      request,
      "index.html",
      context={
          "initial_sessions": initial_sessions,
          "active_session": active_session,
          "thread_view": thread_view,
          "thread_thinking": thread_thinking,
          "pending_draft": pending_draft,
          "event_count": event_count,
          "session_bootstrap": session_bootstrap,
          "backend_options": cfg.backends.options,
          "voice_backends":
              [
                  {
                      "id": backend.id,
                      "label": backend.label,
                      "live_partials": backend.live_partials,
                      "unavailable_reason": backend.unavailable_reason(),
                  } for backend in build_transcription_backends(cfg)
              ],
          "voice_default_backend": cfg.voice.default_backend,
          "active_backend": active_backend,
          "active_backend_label": active_backend_label,
          "active_backend_type": active_backend_type,
          "load_errors": load_errors,
          "auth_enabled": bool(configured_access_key()),
          "hostname": socket.gethostname(),
          "sessions_root": str(cfg.sessions_dir),
          "version": _git_version(),
          "static_asset_version": _static_asset_version(),
          "preview_mode": is_preview_mode(),
      })
