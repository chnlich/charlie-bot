"""Session management API routes."""

import asyncio
from datetime import UTC, datetime
from pathlib import Path

import structlog
from fastapi import APIRouter, Depends, HTTPException, Query
from fastapi.responses import FileResponse
from pydantic import BaseModel
from starlette.responses import Response

from src.api.cron import TaskUpdate, apply_task_yaml_update, next_run_iso
from src.api.deps import (
    get_plan_manager,
    get_session_manager,
    get_thread_manager,
    get_trigger_manager,
    require_found,
    require_session,
)
from src.api.message_utils import (
    SessionBootstrapData,
    build_session_bootstrap_data,
    build_session_view_data,
    events_to_messages,
)
from src.api.responses import FastJsonResponse
from src.api.threads import _thread_list_item
from src.core.chat_events import chat_events_path
from src.core.config import (
    CharlieBotConfig,
    claude_config_dir,
    get_config,
    get_scheduled_tasks,
)
from src.core.event_types import BACKEND_SWITCHED
from src.core.models import (
    BackendOption,
    BackendType,
    CreateSessionRequest,
    DeleteGroupRequest,
    EloneSessionRequest,
    ForkSessionRequest,
    RateRoundRequest,
    RenameGroupRequest,
    RenameSessionRequest,
    SessionMetadata,
    SessionStatus,
    SetGroupRequest,
    SwitchBackendRequest,
    ThreadMetadata,
)
from src.core.plans import PlanRegistryManager
from src.core.sessions import (
    ELONE_BOOTSTRAP_OPENER,
    FORK_BOOTSTRAP_OPENER,
    ScheduledSessionBusyError,
    SessionManager,
    SuccessionRefused,
)
from src.core.threads import ThreadManager

log = structlog.get_logger()
router = APIRouter()


def _default_backend_id(cfg: CharlieBotConfig) -> str:
  return cfg.backend_options[0].id if cfg.backend_options else "claude"


def _active_backend_payload(meta: SessionMetadata, cfg: CharlieBotConfig) -> dict:
  active_backend = meta.backend or _default_backend_id(cfg)
  active_backend_opt = cfg.get_backend_option(active_backend)
  # A cron-dedicated role-carrying (PM) session's backend is controlled by the
  # task yaml: the switch is a write-through that rotates the session, so the
  # payload offers every backend option and flags that switching rotates.
  # Same trigger condition as the write-through guard in switch_session_backend.
  rotates = bool(meta.scheduled_task and meta.role is not None)
  if rotates:
    switchable = [opt.id for opt in cfg.backend_options]
  else:
    switchable = _switchable_backend_ids(active_backend, cfg)
  return {
      "active_backend": active_backend,
      "active_backend_type": active_backend_opt.type if active_backend_opt else "",
      "switchable_backends": switchable,
      "backend_switch_rotates": rotates,
  }


def _bootstrap_payload(bootstrap: SessionBootstrapData, cfg: CharlieBotConfig) -> dict:
  payload = {
      "session": bootstrap.session.model_dump(mode="json"),
      "messages": bootstrap.messages,
      "pending_draft": bootstrap.pending_draft,
      "event_count": bootstrap.total_event_count,
      "oldest_message_ordinal": bootstrap.oldest_message_ordinal,
      "has_more": bootstrap.has_more,
  }
  payload.update(_active_backend_payload(bootstrap.session, cfg))
  return payload


def _backend_domain(option: BackendOption) -> str | None:
  """The resume domain an option belongs to, or None for non-cc-claude backends.

  cc-claude options share a resume domain exactly when they resolve to the same
  ``claude_config_dir`` (each account has its own transcript store). Every other
  backend family is its own, non-switchable domain.
  """
  if option.type != BackendType.CC_CLAUDE:
    return None
  return str(claude_config_dir(option))


def _same_backend_domain(cur_id: str, tgt_id: str, cfg: CharlieBotConfig) -> bool:
  """True when cur and tgt are cc-claude options in the same resume domain.

  ``cur`` must already be the effective current backend (the API layer resolves
  the default). Any non-cc-claude effective option has no switchable domain.
  """
  cur = cfg.get_backend_option(cur_id)
  tgt = cfg.get_backend_option(tgt_id)
  if cur is None or tgt is None:
    return False
  cur_domain = _backend_domain(cur)
  tgt_domain = _backend_domain(tgt)
  return cur_domain is not None and cur_domain == tgt_domain


def _switchable_backend_ids(
    active_backend: str,
    cfg: CharlieBotConfig,
) -> list[str]:
  """Return backend option ids available for this session.

  The list is empty when the effective backend is missing from config or lies
  outside the cc-claude domain (nothing is switchable from it in place);
  otherwise it contains the same-domain ids in config order.
  """
  active_domain = _backend_domain_for(active_backend, cfg)
  if active_domain is None:
    return []
  return [
      opt.id
      for opt in cfg.backend_options
      if opt.type == BackendType.CC_CLAUDE and str(claude_config_dir(opt)) == active_domain
  ]


def _backend_domain_for(backend_id: str, cfg: CharlieBotConfig) -> str | None:
  """The cc-claude resume domain for a backend id, or None if not switchable."""
  opt = cfg.get_backend_option(backend_id)
  if opt is None:
    return None
  return _backend_domain(opt)


def _resolve_requested_backend(
    requested_backend: str | None,
    cfg: CharlieBotConfig,
    *,
    fallback_backend: str | None = None,
) -> str:
  """Resolve a backend override with codex-family alias support.

  Raises HTTPException(400) for any non-None backend id -- whether it arrives
  explicitly via ``requested_backend`` or is inherited via ``fallback_backend``
  -- that isn't a member of ``cfg.backend_options`` and doesn't resolve through
  the codex-family alias below.
  """
  valid_backend_ids = {opt.id for opt in cfg.backend_options}
  resolved_fallback = fallback_backend or _default_backend_id(cfg)

  if requested_backend is not None and requested_backend in valid_backend_ids:
    log.info("using_requested_backend", backend=requested_backend)
    return requested_backend

  if requested_backend is not None and requested_backend.startswith("codex"):
    codex_option = next((opt for opt in cfg.backend_options if opt.type == BackendType.CODEX), None)
    if codex_option:
      log.info("using_requested_backend_family_match", requested=requested_backend, backend=codex_option.id)
      return codex_option.id

  if requested_backend is not None:
    log.warning(
        "invalid_backend_requested",
        requested=requested_backend,
        valid=list(valid_backend_ids),
        fallback=resolved_fallback,
    )
    raise HTTPException(
        status_code=400,
        detail=f"backend '{requested_backend}' is not a recognized backend id; valid ids: {sorted(valid_backend_ids)}",
    )

  if resolved_fallback not in valid_backend_ids:
    log.warning(
        "invalid_fallback_backend",
        fallback=resolved_fallback,
        valid=list(valid_backend_ids),
    )
    raise HTTPException(
        status_code=400,
        detail=f"backend '{resolved_fallback}' is not a recognized backend id; valid ids: {sorted(valid_backend_ids)}",
    )

  log.info("using_fallback_backend", reason="backend_is_none", requested=None, fallback=resolved_fallback)
  return resolved_fallback


@router.get("/", response_model=list[SessionMetadata])
async def list_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  return await session_mgr.list_sessions(
      status=SessionStatus.ACTIVE,
      scheduled=False,
      include_running_status=True,
      include_pending_trigger_status=True,
      include_pending_plan_approval=True,
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
  cfg = get_config()
  return await asyncio.to_thread(cfg.discover_repos)


class ArchivedGroupCount(BaseModel):
  group: str | None
  total: int


class ArchivedSessionsPage(BaseModel):
  sessions: list[SessionMetadata]
  has_more: bool
  next_before: str | None
  next_before_id: str | None
  groups: list[ArchivedGroupCount]


@router.get("/archived", response_model=ArchivedSessionsPage)
async def list_archived_sessions(
    group: str | None = None,
    limit: int = 100,
    before: str | None = None,
    before_id: str | None = None,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """One keyset page of archived sessions, newest first, with group aggregates for the filter strip."""
  try:
    return await session_mgr.list_archived_page(group=group, limit=limit, before=before, before_id=before_id)
  except ValueError as e:
    raise HTTPException(status_code=422, detail=str(e)) from e


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
  return await session_mgr.list_group_names()


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
async def list_scheduled_sessions(session_mgr: SessionManager = Depends(get_session_manager)):
  """List sessions with a scheduled task, newest first."""
  sessions = await session_mgr.list_sessions(
      status=SessionStatus.ACTIVE,
      scheduled=True,
      include_running_status=True,
      include_pending_trigger_status=True,
  )
  task_map = {t.name: t for t in get_scheduled_tasks()}
  now_utc = datetime.now(UTC)
  for s in sessions:
    task = task_map.get(s.scheduled_task)
    if task:
      s.schedule_cron = task.cron
      s.schedule_enabled = task.enabled
      s.schedule_timezone = task.timezone
      s.schedule_project = task.project
      s.schedule_allow_failure = task.allow_failure
      s.schedule_next_run = next_run_iso(task.cron, task.timezone, now_utc)
    else:
      s.schedule_enabled = False
  return sessions


def _parse_session_ids(ids: str) -> list[str]:
  """Split the `ids` query parameter into a deduplicated, order-preserving id list."""
  parsed: list[str] = []
  seen: set[str] = set()
  for part in ids.split(','):
    sid = part.strip()
    if not sid or sid in seen:
      continue
    seen.add(sid)
    parsed.append(sid)
  if not parsed:
    raise HTTPException(status_code=422, detail="ids must name at least one session")
  return parsed


async def _load_requested_sessions(session_mgr: SessionManager, ids: str) -> list[SessionMetadata]:
  """Resolve the requested ids to metadata, dropping ids that no longer exist."""
  loaded = await asyncio.gather(*(session_mgr.get_session(sid) for sid in _parse_session_ids(ids)))
  return [meta for meta in loaded if meta is not None]


@router.get('/status')
async def all_sessions_status(
    ids: str = Query(..., description="Comma-separated ids of the sessions the sidebar is rendering"),
    force: bool = False,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """Return derived sidebar state for the requested sessions.

  Clean sessions are served from the in-process snapshot with zero disk
  access; only sessions whose probed state changed since the last poll are
  re-probed from disk. Pass ``force=1`` to skip the dirty check and re-probe
  every requested session (a full probe also runs on every 10th poll as a
  self-heal fallback).
  """
  sessions = await _load_requested_sessions(session_mgr, ids)
  if not sessions:
    return {}
  await session_mgr.populate_sidebar_state(
      sessions,
      include_running_status=True,
      include_pending_trigger_status=True,
      include_pending_plan_approval=True,
      force=force,
  )
  result: dict[str, dict] = {}
  for meta in sessions:
    result[meta.id] = {
        "has_unread": bool(meta.has_unread),
        "has_running_tasks": meta.has_running_tasks,
        "thinking_since": meta.thinking_since.isoformat() if meta.thinking_since else None,
        "has_pending_trigger": meta.has_pending_trigger,
        "pending_trigger_count": meta.pending_trigger_count,
        "next_trigger_at": meta.next_trigger_at.isoformat() if meta.next_trigger_at else None,
        "has_pending_plan_approval": meta.has_pending_plan_approval,
    }
  # The sidebar's 3 s poll is this host's second-busiest route; FastJsonResponse
  # skips the jsonable_encoder pass FastAPI runs on mapped returns (the
  # message-page cost reason in get_session_events_page).
  return FastJsonResponse(result)


@router.get('/tui/status')
async def tui_status_all(
    ids: str = Query(..., description="Comma-separated ids of the sessions the sidebar is rendering"),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Return tmux liveness and recent Claude jsonl activity for the requested tui-cli sessions."""
  sessions = await _load_requested_sessions(session_mgr, ids)
  tui_sessions = []
  for meta in sessions:
    option = cfg.get_backend_option(meta.backend)
    if option is not None and option.type == BackendType.TUI_CLI:
      tui_sessions.append(meta)
  if not tui_sessions:
    return {}

  from src.agents.backends.tui import _claude_jsonl_busy, tmux_session_exists
  statuses = await asyncio.gather(*(tmux_session_exists(meta.id) for meta in tui_sessions))
  busy_flags = await asyncio.gather(
      *(
          asyncio.to_thread(_claude_jsonl_busy, meta.id) if running else _not_busy()
          for meta, running in zip(tui_sessions, statuses, strict=True)))
  return {
      meta.id: {
          "running": running,
          "busy": busy
      } for meta, running, busy in zip(tui_sessions, statuses, busy_flags, strict=True)
  }


async def _not_busy() -> bool:
  return False


@router.post('/{session_id}/tui/stop')
async def stop_tui(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    cfg: CharlieBotConfig = Depends(get_config),
):
  option = cfg.get_backend_option(meta.backend)
  if option is None or option.type != BackendType.TUI_CLI:
    raise HTTPException(status_code=400, detail="Session backend is not tui-cli")

  from src.agents.backends.tui import kill_tmux_session
  await kill_tmux_session(session_id)
  return {"stopped": True}


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
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Return data needed to render a session chat panel (SPA switch).

  Uses tail-loading: only the last 40 messages are parsed and returned.
  The response includes has_more and oldest_message_ordinal so the frontend
  can paginate backwards.
  """
  await session_mgr.populate_sidebar_state(
      [meta],
      include_running_status=True,
      include_pending_trigger_status=True,
  )
  view = await build_session_view_data(session_id, session_mgr, thread_mgr)
  trigger_mgr = get_trigger_manager()
  triggers = await trigger_mgr.list_triggers(session_id)
  active_backend = meta.backend or (cfg.backend_options[0].id if cfg.backend_options else "claude")
  active_backend_opt = cfg.get_backend_option(active_backend)
  active_backend_type = active_backend_opt.type if active_backend_opt else ""
  # FastJsonResponse for the message-page cost reason in get_session_events_page.
  # The workers tab paints one CSS-truncated description line per card and its
  # full-text modal fetches the thread row on click (the workers-panel list's
  # truncation contract), so the view ships the same prefixed rows — the
  # worst session's whole-row dumps measured 2.6 MB of body per session open.
  return FastJsonResponse(
      {
          "session": meta.model_dump(mode="json"),
          "messages": view.messages,
          "pending_draft": view.pending_draft,
          "threads": [_thread_list_item(t) for t in view.threads],
          "triggers": [tr.model_dump(mode="json") for tr in triggers],
          "event_count": view.total_event_count,
          "oldest_message_ordinal": view.oldest_message_ordinal,
          "usage": view.usage,
          "active_backend": active_backend,
          "active_backend_type": active_backend_type,
          "has_more": view.has_more,
      })


@router.get('/{session_id}/bootstrap')
async def get_session_bootstrap(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Return the minimal data needed to make one chat session usable."""
  bootstrap = await build_session_bootstrap_data(session_id, session_mgr)
  # FastJsonResponse for the message-page cost reason in get_session_events_page.
  return FastJsonResponse(_bootstrap_payload(bootstrap, cfg))


@router.get('/{session_id}/usage')
async def get_session_usage(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Return lazy session status and usage data for the active header."""
  usage = await session_mgr.resolve_session_usage(session_id, meta)
  payload = {
      "session": meta.model_dump(mode="json"),
      "usage": usage,
  }
  payload.update(_active_backend_payload(meta, cfg))
  return payload


@router.get('/{session_id}/events')
async def get_session_events_page(
    session_id: str,
    before: int,
    limit: int = 40,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """Paginate backwards through session messages by message ordinal.

  ``before`` is a message ordinal (exclusive upper bound). ``limit`` is the
  minimum number of MESSAGES per page (default 40, clamped 1..200) — the page
  start is snapped back to its turn start, so a page holds at least ``limit``
  messages unless the history below ``before`` is exhausted. Returns
  ``{"messages", "has_more", "next_before"}`` where messages are ascending,
  ``next_before`` is the snapped page start, and ``has_more = next_before > 0``.

  Sessions with ``archive_offset > 0`` fall back entirely to the legacy
  event-index cursor path (``load_chat_events_range`` + ``events_to_messages``)
  and never mix the two cursor domains.
  """
  limit = max(1, min(limit, 200))
  # FastJsonResponse skips the jsonable_encoder pass FastAPI runs on mapped
  # returns; on this payload (211 messages / 559 KB) that pass measures ~3x a
  # plain json.dumps, and every field is already a plain parsed-JSON type so
  # the dumped body is unchanged.
  if meta.archive_offset == 0:
    projection = await asyncio.to_thread(session_mgr.get_message_projection, session_id)
    if projection is not None:
      messages, next_before, has_more = projection.slice_before(before, limit)
      return FastJsonResponse({"messages": messages, "has_more": has_more, "next_before": next_before})
  start = max(0, before - limit)
  events, has_more = await asyncio.to_thread(session_mgr.load_chat_events_range, session_id, start, before)
  messages = events_to_messages(events, event_index_offset=start)
  return FastJsonResponse({"messages": messages, "has_more": has_more, "next_before": start})


@router.get('/{session_id}/recap')
async def get_session_recap(
    session_id: str,
    upto: int | None = None,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  """Pure-extraction recap (no LLM) plus any cached Haiku summary for a divider.

  ``upto`` is a global event_index (default: latest). Returns ordered asks, the
  last exchange, the cached summary (or null), and whether that summary is stale.
  """
  from src.core import recap
  if upto is None:
    count = await asyncio.to_thread(session_mgr.get_chat_event_count_sync, session_id)
    upto = max(0, count - 1)
  extract = await asyncio.to_thread(recap.extract_recap, session_mgr, session_id, upto)
  summary, stale = await asyncio.to_thread(recap.lookup_cached_summary, session_mgr, session_id, upto)
  return {**extract, "summary": summary, "summary_stale": stale}


@router.post('/{session_id}/recap/summarize')
async def summarize_session_recap(
    session_id: str,
    upto: int,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Generate (via a light backend), cache, and return the recap summary for a divider."""
  from src.core import recap
  summary = await recap.generate_and_cache_summary(session_mgr, session_id, upto, cfg)
  return {"summary": summary}


def _reference_instructions(reference_path: Path) -> str:
  return (
      f"The full prior conversation up to the takeover point is in {reference_path}. "
      "Entries are chronological, with newest entries at the end. The file may be large, so it does not "
      "need to be read in full; read what is needed to reconstruct the current state.\n\n")


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
  try:
    meta = await session_mgr.fork_session(
        session_id,
        event_index=body.event_index if body else None,
        backend=backend,
    )
  except FileNotFoundError as e:
    raise HTTPException(status_code=404, detail="Session not found") from e
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e

  reference_path = session_mgr.parent_reference_path(meta.id)
  bootstrap_prompt = (
      f"{FORK_BOOTSTRAP_OPENER}\n\n"
      f"{_reference_instructions(reference_path)}"
      "Get oriented from that reference, summarize where things stand, and wait for the user's next instruction.")

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
  try:
    meta = await session_mgr.elone_session(session_id, body.event_index, backend=backend)
  except FileNotFoundError as e:
    raise HTTPException(status_code=404, detail="Session not found") from e
  except ScheduledSessionBusyError as e:
    raise HTTPException(status_code=409, detail=str(e)) from e
  except SuccessionRefused as e:
    raise HTTPException(status_code=409, detail=str(e)) from e
  except ValueError as e:
    raise HTTPException(status_code=400, detail=str(e)) from e

  reference_path = session_mgr.parent_reference_path(meta.id)
  bootstrap_prompt = (
      f"{ELONE_BOOTSTRAP_OPENER} "
      "The dissatisfaction is usually with the most recent exchange before the takeover point.\n\n"
      f"{_reference_instructions(reference_path)}"
      "Understand what the user wanted and where it went wrong, then give your read and a better approach. "
      "Confirm with the user before acting.")

  # Write synthetic user event and auto-start the assistant
  from src.api.chat import run_and_finalize
  from src.core.tasks import create_logged_task
  create_logged_task(run_and_finalize(cfg, meta, bootstrap_prompt, session_mgr, skip_user_event=False))

  return meta


@router.post("/{session_id}/backend", response_model=SessionMetadata)
async def switch_session_backend(
    session_id: str,
    body: SwitchBackendRequest,
    parent: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config),
):
  """Switch a session's backend, in place or via write-through rotation.

  ``meta.backend`` is an effective current backend: the raw field when set,
  else ``backend_options[0]``. For an ordinary (or role-less dedicated) session,
  only cc-claude options sharing the same ``claude_config_dir`` are switchable
  in place; a session targeting anything else must fork/clone. For a
  cron-dedicated role-carrying (PM) session the yaml is the single control
  point, so the request writes through to the task yaml and returns the rotated
  session (whose ``id`` can differ from the path id), reusing the cron editor's
  implementation.
  """
  valid_ids = {opt.id for opt in cfg.backend_options}
  if body.backend not in valid_ids:
    raise HTTPException(
        status_code=400,
        detail=(
            f"backend '{body.backend}' is not a recognized backend id. To switch to a "
            "backend outside the session's domain, clone/fork the session with the "
            "target backend instead."),
    )

  effective_current = parent.backend or _default_backend_id(cfg)
  if body.backend == effective_current:
    return parent

  # The yaml is the single control point for a cron-dedicated PM session's
  # backend: the scheduler reconciles the dedicated session against the task
  # file on every fire, so an in-place switch here would be overridden at the
  # next fire (or trigger an unintended generation rotation). Write through
  # instead: update the task yaml's backend key and rotate to the fresh
  # session, reusing the cron editor's implementation. The returned session's
  # id can differ from the path id; nothing in this class's prior session is
  # resumed (rotation creates a fresh session), so no resume-domain check runs.
  if parent.scheduled_task and parent.role is not None:
    _, rotated = await apply_task_yaml_update(
        parent.scheduled_task,
        TaskUpdate(backend=body.backend),
        cfg,
        session_mgr,
    )
    return rotated

  if not _same_backend_domain(effective_current, body.backend, cfg):
    raise HTTPException(
        status_code=400,
        detail=(
            f"backend '{body.backend}' cannot be switched to in place: it does not "
            "share this session's resume domain (backend types differ or config dirs "
            "differ). Clone/fork the session with the target backend instead."),
    )

  previous = effective_current
  meta = require_found(await session_mgr.switch_backend(session_id, body.backend))

  audit_event = {
      "type": BACKEND_SWITCHED,
      "from": previous,
      "to": body.backend,
  }
  await session_mgr.persist_and_broadcast(session_id, audit_event)
  return meta


@router.get("/{session_id}", response_model=SessionMetadata)
async def get_session(meta: SessionMetadata = Depends(require_session)):
  return meta


@router.delete("/{session_id}", response_model=SessionMetadata)
async def archive_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  event_count = await asyncio.to_thread(session_mgr.get_chat_event_count_sync, session_id, meta)
  if event_count == 0:
    await session_mgr.delete_session_permanently(session_id)
    return meta

  return require_found(await session_mgr.archive_session(session_id))


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
  return require_found(await session_mgr.star_session(session_id))


@router.post("/{session_id}/unstar", response_model=SessionMetadata)
async def unstar_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)):
  return require_found(await session_mgr.unstar_session(session_id))


@router.post("/{session_id}/rounds/{round_id}/rate", response_model=SessionMetadata)
async def rate_round(
    session_id: str,
    round_id: str,
    req: RateRoundRequest,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
):
  if req.rating is None:
    meta.round_ratings.pop(round_id, None)
  else:
    meta.round_ratings[round_id] = req.rating
  meta.updated_at = datetime.now(UTC)
  await session_mgr.save_metadata(meta)
  log.info("round_rated", session_id=session_id, round_id=round_id, rating=req.rating)
  return meta


@router.patch("/{session_id}", response_model=SessionMetadata)
async def rename_session(
    session_id: str,
    req: RenameSessionRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  return require_found(await session_mgr.rename_session(session_id, req.name))


@router.post("/{session_id}/group", response_model=SessionMetadata)
async def set_session_group(
    session_id: str,
    req: SetGroupRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
):
  return require_found(await session_mgr.set_group(session_id, req.group))


@router.get("/{session_id}/events.jsonl")
async def get_events_jsonl(session_id: str):
  """Serve the raw chat_events.jsonl file for a session."""
  cfg = get_config()
  path = chat_events_path(cfg.sessions_dir / session_id)
  if not path.exists():
    raise HTTPException(status_code=404, detail="Events file not found")
  return FileResponse(path, media_type="application/x-ndjson")


@router.get("/{session_id}/threads", response_model=list[ThreadMetadata])
async def list_threads(session_id: str, thread_mgr: ThreadManager = Depends(get_thread_manager)):
  return await thread_mgr.list_threads(session_id)


@router.get("/{session_id}/plans")
async def list_plans(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
):
  """Return the plan registry for a session with derived states and read errors.

  Unknown session → 404. Known session → always 200 with ``{"plans": [...], "errors": [...]}``;
  a corrupt registry produces 200 with empty plans and one error entry, never 5xx.
  """
  # The plan panel polls this route; FastJsonResponse skips the jsonable_encoder
  # pass FastAPI runs on mapped returns (the message-page cost reason in
  # get_session_events_page).
  return FastJsonResponse(await plan_mgr.list_plans(session_id))
