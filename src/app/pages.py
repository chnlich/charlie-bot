"""Server-rendered pages — one Jinja2 template per page under web/templates/: the chat
UI, home, events viewer, and token usage. A feature package brings its own pages and
templates through the page-render registry (src.runtime.hooks.page_render)."""

import asyncio
import datetime as dt
import json
import re
import socket
import time
from typing import TYPE_CHECKING
from urllib.parse import urlparse

from fastapi import APIRouter, Depends, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse
from starlette.responses import Response

from src.infra.config import CharlieBotConfig, configured_access_key
from src.infra.constants import (
    AUTH_STATUS_PATH,
    USAGE_SOURCE_CHARLIE_BOT,
    USAGE_SOURCE_CHARLIE_CODE,
    USAGE_SOURCE_CLAUDE_CODE,
    USAGE_SOURCE_CODEX,
    USAGE_SOURCE_OPENCODE,
)
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SessionStatus
from src.infra.timeouts import (
    HOME_SERVICE_PROBE_TIMEOUT,
    USAGE_LEDGER_LOCK_WAIT_SECONDS,
    USAGE_PAGE_CAPTURE_LOCK_WAIT_SECONDS,
)
from src.runtime import templating
from src.runtime.api.deps import (
    SESSION_NOT_FOUND_DETAIL,
    get_config_on_loop,
    get_session_manager,
    get_task_manager,
    get_thread_manager,
)
from src.runtime.api.message_utils import build_session_bootstrap_data
from src.runtime.api.sessions import (
    _bootstrap_payload,
    _default_backend_id,
    apply_row_schedule,
    project_worker_threads,
    row_schedule_fields,
)
from src.runtime.hooks import page_render
from src.runtime.sessions import SessionManager
from src.runtime.task_sessions import TaskTreeManager
from src.runtime.threads import ThreadManager

if TYPE_CHECKING:
  from src.features.usage.usage_ledger import LedgerRow

log = LazyStructlogLogger()

# The app's own destinations, listed first on the home page; feature packages register
# the cards that follow (page_render.register_home_card). The same on every host, so they
# live in code rather than config; each renders as a card linking straight to the page.
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

router = APIRouter()


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

  return templating.templates().TemplateResponse(
      request,
      "events_viewer.html",
      context={
          "session": session,
          "session_id": session_id,
          "events_url": f"/api/sessions/{session_id}/events.jsonl",
          "hostname": socket.gethostname(),
          "static_asset_version": templating.static_asset_version(),
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


def preload_usage_tally_stack() -> None:
  """Import the tally stack the usage page and ledger handlers first-import.

  The modules mirror the lazy sets in ``_capture_ledger_rows``, ``_account_source``
  and the scheduler's ledger handler; keep them in step. A long-lived server that
  starts before a deploy keeps its in-memory modules while a request-time
  first-import reads the newer files, and the mixed-version import raises inside
  the request (the page 500s until restart). Loading here pins the stack to the
  code the server started with. A failure only logs: the request path keeps its
  own import as the loud fallback.
  """
  started = time.monotonic()
  import sqlite3  # noqa: F401  -- the pin is the import itself

  from src.features.usage import token_tally, usage_ledger  # noqa: F401
  log.info("usage_tally_stack_preloaded", duration_ms=round((time.monotonic() - started) * 1000))


def _capture_ledger_rows() -> tuple[list[LedgerRow], dict[str, str], dict[str, int], float, dict[str, object]]:
  """Capture this host's new usage into the ledger, then read the page rows from the ledger
  alone — so the numbers survive deletion of the logs they were parsed from — plus the
  backend registry the rows' charlie-bot accounts attribute against.

  Runs in a thread as the page's single-flight task body. A capture error propagates to the
  awaiting request: the page fails loudly instead of rendering rows the broken capture may
  have left wrong. A capture whose writes time out on the ledger's write lock is the one
  exception: the lock says this load could not add rows, not that the stored rows are wrong,
  so the load skips the capture and serves the stored truth (a rollback-journal writer waits
  out every long reader on this machine — the bot's own workers query the ledger — and the
  request cannot carry the scheduler-capture's 30 s tolerance).
  """
  # The ledger + capture stack (sqlite3, the token_tally walkers) rides the page like
  # croniter rides its next-run resolutions: the M99 server import floor carries no
  # tally stack for a page that may never load.
  import sqlite3

  from src.features.usage.token_tally import backend_registry, capture_local
  from src.features.usage.usage_ledger import UsageLedger, default_ledger_path

  started = time.monotonic()
  with UsageLedger(default_ledger_path()) as ledger:
    ledger.set_lock_wait(USAGE_PAGE_CAPTURE_LOCK_WAIT_SECONDS)
    try:
      written = capture_local(ledger)
    except sqlite3.OperationalError as e:
      if "database is locked" not in str(e):
        raise
      log.warning("token_usage_capture_skipped_locked", error=str(e))
      written = {}
    finally:
      ledger.set_lock_wait(USAGE_LEDGER_LOCK_WAIT_SECONDS)
    rows, native_starts = ledger.model_rows_with_native_starts()
  return rows, native_starts, written, time.monotonic() - started, backend_registry()


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
  from src.features.usage.token_tally import backend_page_source

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
      "ctx":
          ctx,
      "payload":
          payload,
      "window":
          window,
      "window_str":
          f"{window[0]} → {window[1]}" if rows else "",
      "cache_share":
          cache_share,
      "generated":
          dt.datetime.now().astimezone().strftime("%Y-%m-%d %H:%M %Z"),
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
  return templating.templates().TemplateResponse(
      request,
      "token_usage.html",
      context=_token_usage_context(rows, native_starts, written, elapsed_s, registry),
  )


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
  return templating.templates().TemplateResponse(
      request,
      "home.html",
      context={
          "hostname": socket.gethostname(),
          "destinations": (*_HOME_DESTINATIONS, *page_render.home_cards()),
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
  from src.features.session_tree_preview.session_tree_preview import is_preview_mode
  from src.features.voice.transcription.registry import build_transcription_backends
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
          from src.runtime import worker_transcript
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

  return templating.templates().TemplateResponse(
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
          "version": templating.git_version(),
          "static_asset_version": templating.static_asset_version(),
          "preview_mode": is_preview_mode(),
      })
