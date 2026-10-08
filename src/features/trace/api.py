"""The Perfetto trace viewer page and the merged-trace route.

The merged route builds one gzip artifact per distinct input set and serves it from a cache under the
profile. Builds run off the event loop, single-flight per cache key.
"""

import asyncio
import concurrent.futures
import fnmatch
import hashlib
import json
import os
import subprocess
import tempfile
import threading
import types
from collections.abc import Awaitable, Callable
from pathlib import Path
from urllib.parse import urlencode

from fastapi import APIRouter, HTTPException, Query, Request
from fastapi.responses import FileResponse, HTMLResponse

from src.features.trace import direct_pass_child, trace_merge_child
from src.infra.config import get_config
from src.infra.constants import FILE_SERVER_MOUNTS, PERFETTO_MERGED_PATH
from src.infra.log_once import LazyStructlogLogger
from src.runtime import templating
from src.runtime.hooks import wiring

log = LazyStructlogLogger()

router = APIRouter()

# The viewer route path. The auth whitelist (src.runtime.api.auth) does not admit it — it
# reads local trace files, so it sits behind the access key like the file server.
PERFETTO_VIEWER_PATH = "/perfetto"

_PERFETTO_MERGE_CACHE_LIMIT = 24

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


def _trace_merge() -> types.ModuleType:
  """The request-time trace-merge stack, imported on first use and reused after.

  The merge builders and the direct-pass validator are the only consumers; the
  M99 server import floor carries no trace stack.
  """
  import src.features.trace.trace_merge

  return src.features.trace.trace_merge


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

  return templating.templates().TemplateResponse(
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


async def start_service(ctx: wiring.ServiceContext) -> None:
  """Start nothing: the merge pool builds on the first merge."""


async def stop_service() -> None:
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
  argv = trace_merge_child.parent_argv(paths, out_path, slim, str(Path(__file__).resolve().parents[3]))
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
  argv = direct_pass_child.parent_argv(path, out_path, str(Path(__file__).resolve().parents[3]))
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
