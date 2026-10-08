"""Session management API routes."""

import asyncio
import json
import uuid
from collections.abc import Iterable
from datetime import UTC, datetime
from pathlib import Path
from typing import get_args

from fastapi import APIRouter, Depends, HTTPException, Query, Request
from fastapi.responses import FileResponse, JSONResponse
from pydantic import BaseModel, TypeAdapter, field_validator
from starlette.responses import Response

from src.backends.claude_code import claude_accounts
from src.features.artifacts.plans import PlanRegistryManager
from src.features.cron.api import next_run_iso
from src.infra import event_types as ET
from src.infra.compression import gzip_level1
from src.infra.config import CharlieBotConfig, get_config, scheduled_tasks_snapshot
from src.infra.constants import BackendType
from src.infra.event_types import BACKEND_SWITCHED
from src.infra.log_once import LazyStructlogLogger
from src.infra.memo import BoundedMemo, StatSignatureMemo
from src.infra.models import (
    AcknowledgeTaskInputsRequest,
    AncestorRef,
    CancelRunRequest,
    CancelTaskRequest,
    CompleteTaskRequest,
    CreateSessionRequest,
    DeleteGroupRequest,
    EloneSessionRequest,
    ExplainRequest,
    ForkSessionRequest,
    PatchSessionTaskRequest,
    RateRoundRequest,
    RenameGroupRequest,
    RetryRunRequest,
    RunCancelResponse,
    RunKind,
    RunPage,
    RunRow,
    SessionMetadata,
    SessionRow,
    SessionStatus,
    SetGroupRequest,
    SwitchBackendRequest,
    TaskState,
    ThreadMetadata,
    TriggerStatus,
    UtcDatetime,
    WorkerThreadRef,
    WorkState,
)
from src.infra.responses import (
    GZIP_RESPONSE_HEADERS,
    FastJsonResponse,
    PreencodedJSONResponse,
    fast_json_bytes,
    gzip_body_response,
    gzip_file_fresh,
    request_wants_gzip,
)
from src.runtime import sidebar_state, thinking_state
from src.runtime.api.deps import (
    SESSION_NOT_FOUND_DETAIL,
    bad_request,
    get_config_on_loop,
    get_plan_manager,
    get_run_store,
    get_session_manager,
    get_task_manager,
    get_thread_manager,
    get_trigger_manager,
    require_caller,
    require_found,
    require_session,
)
from src.runtime.api.message_utils import (
    SessionBootstrapData,
    build_session_bootstrap_data,
    events_to_messages,
    get_message_projection_fast,
)
from src.runtime.api.threads import view_thread_rows
from src.runtime.chat_events import chat_events_path
from src.runtime.control_events import sha256_hex
from src.runtime.message_aggregator import tool_preview
from src.runtime.run_token import CallerIdentity
from src.runtime.runs import RunIdentityConflictError, RunNotFoundError, run_not_found_in_task_text
from src.runtime.scheduled_sessions import cron_subtree_roots
from src.runtime.session_dispatch import agent_provenance, input_event_type_for_caller
from src.runtime.sessions import ELONE_BOOTSTRAP_OPENER, FORK_BOOTSTRAP_OPENER, HISTORY_LOCATION_NOTE, SessionManager
from src.runtime.spawner_backends import EMPTY_BACKENDS_OPTIONS_REFUSAL
from src.runtime.takeoff_gate import DelegationBlockedError
from src.runtime.task_sessions import (
    AGENT_CREATE_SCOPE_REFUSAL,
    TASK_CREATE_REQUEST_ID_REQUIRED,
    TaskArchivedError,
    TaskConflictError,
    TaskForbiddenError,
    TaskInvalidError,
    TaskNotFoundError,
    TaskTreeManager,
    not_task_node_detail,
)
from src.runtime.thinking_state import run_backend
from src.runtime.threads import ThreadManager
from src.runtime.triggers import TriggerManager

log = LazyStructlogLogger()
router = APIRouter()

# The search route's read-only overlay serializes derived datetimes through the
# model's own JSON scheme: a hand-rolled isoformat() emits +00:00 where the
# model's UtcDatetime fields emit Z.
_UTC_DATETIME_JSON = TypeAdapter(UtcDatetime | None)

# Read from the Literal so the preview's 400 cannot drift from the type home.
_RUN_KINDS = frozenset(get_args(RunKind))


def _default_backend_id(cfg: CharlieBotConfig) -> str:
  return cfg.backends.options[0].id if cfg.backends.options else "claude"


def _active_backend_payload(meta: SessionMetadata, cfg: CharlieBotConfig) -> dict:
  # A worker node displays its newest Run's backend (the delegation's target
  # model), never the inherited creation value the persisted field carries.
  from src.features.cron.cron_sequence import bound_task_name
  active_backend = (meta.run_backend or meta.backend) or _default_backend_id(cfg)
  active_backend_opt = cfg.get_backend_option(active_backend)
  return {
      "active_backend":
          active_backend,
      "active_backend_type":
          active_backend_opt.type if active_backend_opt else "",
      "switchable_backends":
          _switchable_backend_ids(active_backend, cfg, dedicated=bound_task_name(meta.id) is not None),
  }


# The bootstrap payload's tool rows render through tool_preview: the output and
# the input fields the renderer reads cap at TOOL_PREVIEW_CHARS with their
# truncation markers set, and the input's other string values carry the
# dead-field bound — one home shared with the stream delta's tool shape.
# The projection memo's dicts stay shared with the events pages and the M26
# digest, so a message copies only when one of its tools actually trims.


def _bootstrap_tool_previews(messages: list[dict]) -> list[dict]:
  """The bootstrap payload's messages with tool input/output trimmed to the
  preview cap."""
  out = []
  for msg in messages:
    tools = msg.get("tools") if isinstance(msg, dict) else None
    if not isinstance(tools, list):
      out.append(msg)
      continue
    previews = [tool_preview(tool) if isinstance(tool, dict) else tool for tool in tools]
    if all(new is old for new, old in zip(previews, tools, strict=True)):
      out.append(msg)
      continue
    trimmed = dict(msg)
    trimmed["tools"] = previews
    out.append(trimmed)
  return out


def _bootstrap_payload(bootstrap: SessionBootstrapData, cfg: CharlieBotConfig) -> dict:
  payload = {
      "session": bootstrap.session.model_dump(mode="json", exclude=_RESPONSE_ROW_EXCLUDE),
      "messages": _bootstrap_tool_previews(bootstrap.messages),
      "pending_draft": bootstrap.pending_draft,
      "event_count": bootstrap.total_event_count,
      "oldest_message_ordinal": bootstrap.oldest_message_ordinal,
      "has_more": bootstrap.has_more,
  }
  payload.update(_active_backend_payload(bootstrap.session, cfg))
  return payload


def _switchable_backend_ids(
    active_backend: str,
    cfg: CharlieBotConfig,
    *,
    dedicated: bool,
) -> list[str]:
  """Return backend option ids available for this session, in config order.

  An ordinary session accepts every configured option: a cross-family target
  starts its own native conversation and catches up from the session's chat
  log. A task-bound node keeps the in-domain restriction — the scheduler
  re-aligns it to its task config on every tick, so only the ids in its
  continuation domain (``claude_accounts.continuation_domain``) are offered; a
  bound non-Claude node therefore lists only itself. The list is empty when
  the effective backend is missing from config.
  """
  active_option = cfg.get_backend_option(active_backend)
  if active_option is None:
    return []
  if not dedicated:
    return [opt.id for opt in cfg.backends.options]
  active_domain = claude_accounts.continuation_domain(active_option, cfg)
  return [opt.id for opt in cfg.backends.options if claude_accounts.continuation_domain(opt, cfg) == active_domain]


# Wire spelling of the 400 an id outside cfg.backends.options earns: all three
# raisers (_resolve_requested_backend's two and the switch route) format the
# same sentence.
_UNKNOWN_BACKEND_DETAIL = "backend '{}' is not a recognized backend id; valid ids: {}"

# Shared Query description: the routes exposing the sidebar's ids parameter must
# carry word-identical OpenAPI metadata, so the text has one copy here.
_SIDEBAR_IDS_QUERY_DESC = "Comma-separated ids of the sessions the sidebar is rendering"


def _resolve_requested_backend(
    requested_backend: str | None,
    cfg: CharlieBotConfig,
    *,
    fallback_backend: str | None,
) -> str:
  """Resolve a backend override with codex-family alias support.

  Raises HTTPException(400) for any non-None backend id -- whether it arrives
  explicitly via ``requested_backend`` or is inherited via ``fallback_backend``
  -- that isn't a member of ``cfg.backends.options`` and doesn't resolve through
  the codex-family alias below.
  """
  valid_backend_ids = {opt.id for opt in cfg.backends.options}
  resolved_fallback = fallback_backend or _default_backend_id(cfg)

  if requested_backend is not None and requested_backend in valid_backend_ids:
    log.info("using_requested_backend", backend=requested_backend)
    return requested_backend

  if requested_backend is not None and requested_backend.startswith("codex"):
    codex_option = next((opt for opt in cfg.backends.options if opt.type == BackendType.CODEX), None)
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
        detail=_UNKNOWN_BACKEND_DETAIL.format(requested_backend, sorted(valid_backend_ids)),
    )

  if resolved_fallback not in valid_backend_ids:
    log.warning(
        "invalid_fallback_backend",
        fallback=resolved_fallback,
        valid=list(valid_backend_ids),
    )
    raise HTTPException(
        status_code=400,
        detail=_UNKNOWN_BACKEND_DETAIL.format(resolved_fallback, sorted(valid_backend_ids)),
    )

  log.info("using_fallback_backend", reason="backend_is_none", requested=None, fallback=resolved_fallback)
  return resolved_fallback


# ---------------------------------------------------------------------------
# Projected legacy worker-thread rows (sidebar list responses)
# ---------------------------------------------------------------------------
# A legacy session (profile None) stays a root row of every sidebar list; the
# worker threads its delegations left under threads/*/metadata.json project as
# read-only worker-leaf rows under it. The projected rows are response-only
# SessionMetadata objects — worker_thread marks them and the transient
# exclusion keeps every metadata write free of it — derived from the memoized
# full thread-row scan (view_thread_rows: every threads/*/metadata.json, no
# time window; never init_worker_recovery's 30-day windowed badge scan).
# Each row memoizes on (parent id, thread id) against the parent row object
# and the thread row object, both shared cache references whose identity
# changes exactly when their file changed, so the search route's whole-body
# memo keeps serving while nothing moved.
_PROJECTED_ROW_MEMO_LIMIT = 8192
_projected_row_memo: BoundedMemo[tuple[str, str], tuple[SessionMetadata, dict,
                                                        SessionMetadata]] = BoundedMemo(_PROJECTED_ROW_MEMO_LIMIT)

# TaskSpec.goal is the task's prompt prose — tens of KB per task node — and its
# one reader is the task-context modal through GET /{session_id}, so every
# poll, switch, and listing payload ships the spec without the body; the detail
# render keeps it.
_RESPONSE_ROW_EXCLUDE = {"task": {"goal"}}


def _datetime_from_epoch_ms(ms: int) -> datetime:
  return datetime.fromtimestamp(ms / 1000, tz=UTC)


def _projected_thread_row(parent: SessionMetadata, thread_row: dict) -> SessionMetadata:
  """One legacy worker thread projected as a sidebar worker-leaf row."""
  key = (parent.id, str(thread_row["id"]))
  hit = _projected_row_memo.get(key)
  if hit is not None and hit[0] is parent and hit[1] is thread_row:
    return hit[2]
  projected = SessionMetadata(
      id=str(thread_row["id"]),
      name=str(thread_row["description"] or "")[:80],
      status=parent.status,
      profile="worker",
      task_parent_id=parent.id,
      created_at=_datetime_from_epoch_ms(thread_row["created_at"]),
      updated_at=_datetime_from_epoch_ms(
          thread_row["completed_at"] or thread_row["started_at"] or thread_row["created_at"]),
      has_running_tasks=thread_row["status"] == "running",
      backend=thread_row["backend"] or parent.backend,
      worker_thread=WorkerThreadRef(session_id=parent.id, thread_id=str(thread_row["id"])),
  )
  _projected_row_memo.store(key, (parent, thread_row, projected))
  return projected


async def project_worker_threads(
    rows: list[SessionMetadata],
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
) -> list[SessionMetadata]:
  """Every sidebar list row plus one projected worker leaf per legacy worker thread.

  The parent rows return as given; after each legacy row (profile None) its
  session's thread rows ride ``view_thread_rows`` — the session view's
  memoized full scan — and each becomes one leaf row named for the thread
  description's first 80 characters, with the parent's status and the
  thread's times. Nothing is written to disk.
  """
  out: list[SessionMetadata] = []
  for row in rows:
    out.append(row)
    if row.profile is not None:
      continue
    out.extend(_projected_thread_row(row, thread_row) for thread_row in await view_thread_rows(row.id, cfg, thread_mgr))
  return out


# The model's transient schedule fields ride every row dump as nulls (they were
# the deleted Scheduled listing's overlay slots); the join's answer replaces
# them wholesale, so an unbound row carries none of them.
_SCHEDULE_MODEL_NULLS = (
    "schedule_cron",
    "schedule_timezone",
    "schedule_enabled",
    "schedule_next_run",
    "schedule_project",
    "schedule_allow_failure",
)

# The join's one-entry memo. The fields map is a pure function of the id set
# and the cron snapshot's fingerprint (scheduled_tasks_snapshot returns it, the
# freshness key get_scheduled_tasks itself answers on), except schedule_next_run whose
# answer stays valid until the fire time it names — the _NEXT_RUN_MEMO rule —
# so the entry carries the earliest served fire and re-derives once now
# crosses it. Callers read the map and never mutate it (apply_row_schedule
# updates the row dump from it).
_ROW_SCHEDULE_MEMO: tuple[object, tuple[str, ...], dict[str, dict], datetime] | None = None
# An all-unbound answer holds no time-dependent field, so only the fingerprint
# can retire it.
_ROW_SCHEDULE_NO_FIRE = datetime.max.replace(tzinfo=UTC)


def row_schedule_fields(session_ids: Iterable[str], now_utc: datetime) -> dict[str, dict]:
  """The schedule payload per listed row, keyed on ``bound_task_name`` (plan 4.1).

  The one join every sidebar list producer calls: a node a loaded task binds —
  ``bound_task_name``, the loaded task whose ``session_id`` names the node —
  carries ``schedule_task`` plus the four schedule fields computed from that
  config the way the deleted Scheduled listing computed them; an unbound node
  carries ``schedule_task: null`` and none of the four. ``next_run_iso`` serves
  each occurrence until it passes, so a delivered next run never goes stale.
  One snapshot of the task configs feeds both the predicate and the field
  values, so a hot reload between the two reads cannot split the answer. A
  repeat of an unchanged question (same id set, snapshot fingerprint, and no
  served fire passed) serves the stored map whole.
  """
  ids = tuple(sorted(set(session_ids)))
  global _ROW_SCHEDULE_MEMO
  hit = _ROW_SCHEDULE_MEMO
  tasks, fingerprint = scheduled_tasks_snapshot()
  if (hit is not None and now_utc < hit[3] and hit[1] == ids and fingerprint == hit[0]):
    return hit[2]
  out: dict[str, dict] = {}
  from src.features.cron.cron_sequence import bound_task_name  # the M99 import floor carries no schedule chain
  for session_id in ids:
    task_name = bound_task_name(session_id, tasks)
    if task_name is None:
      out[session_id] = {"schedule_task": None}
      continue
    # bound_task_name answered from this same snapshot, so the config exists.
    task = next(t for t in tasks if t.name == task_name)
    out[session_id] = {
        "schedule_task": task.name,
        "schedule_cron": task.cron,
        "schedule_timezone": task.timezone,
        "schedule_enabled": task.enabled,
        "schedule_next_run": next_run_iso(task.cron, task.timezone, now_utc),
        "schedule_allow_failure": task.allow_failure,
    }
  fires = [
      datetime.fromisoformat(fields["schedule_next_run"])
      for fields in out.values()
      if fields["schedule_task"] is not None
  ]
  _ROW_SCHEDULE_MEMO = (fingerprint, ids, out, min(fires) if fires else _ROW_SCHEDULE_NO_FIRE)
  return out


def apply_row_schedule(dump: dict, fields: dict) -> dict:
  """One listed row's payload: the model dump with the join's schedule fields.

  Drops the model's always-null schedule slots first, so an unbound row carries
  ``schedule_task: null`` and none of the four (the "unbound carries none"
  half of the plan 4.1 table), then applies the bound overlay. Mutates *dump*
  in place and returns it.
  """
  for key in _SCHEDULE_MODEL_NULLS:
    dump.pop(key, None)
  dump.update(fields)
  return dump


# The sidebar's root list renders pre-dumped rows and ships the gzip form from
# a body-keyed memo; the limit covers one steady-state body per open tab.
_SESSIONS_LIST_GZIP_MEMO_LIMIT = 4


class _SessionsListMemos:
  """One sidebar route's three render memos, held as attributes so the shared
  render helper reads and stores through one object per route.

  The Workspace route and the Threads route render the same row shape through
  the same helper, and each owns a private holder — the whole-body slot, the
  per-row render map, and the body-keyed gzip memo — so the two lists never
  evict each other and one route's memo can never serve the other's body.
  """

  def __init__(self) -> None:
    self.gzip_memo: BoundedMemo[bytes, bytes] = BoundedMemo(_SESSIONS_LIST_GZIP_MEMO_LIMIT)
    # One steady-state whole-body slot beside the gzip memo: the search
    # route's _search_whole_body mechanism. The slot is only ever replaced whole.
    self.whole_body: tuple[tuple[SessionMetadata, ...], tuple, bytes] | None = None
    # The changed round's per-row render: row id -> (overlay state, schedule
    # state, row, final payload dict). The slot holds the row, and a live
    # reference pins its id(), so an id hit is that row and only that row; the
    # manager's fresh check moves a row's identity exactly when its content
    # moves, so a slot can never serve a stale row's fields, and the two state
    # tuples in the slot re-state the render's remaining inputs. Payload dicts
    # are handed to the JSON renderer uncopied and never mutated after the
    # schedule join, which is what keeps a shared slot read-only. Pruned to
    # the current projection after each changed round.
    self.row_render: dict[int, tuple[tuple, tuple, SessionMetadata, dict]] = {}


_workspace_list_memos = _SessionsListMemos()
_chat_threads_list_memos = _SessionsListMemos()


async def _active_listing_corpus(session_mgr: SessionManager) -> tuple[list[SessionMetadata], dict[str, dict]]:
  """The active non-scheduled corpus with its status derivations, read-only.

  One home for the fetch both sidebar root lists ride: the Workspace list and
  the Threads view split this one corpus by subtree membership, so a change to
  the corpus definition (a new derivation flag) lands here and reaches both
  lists.
  """
  return await session_mgr.list_sessions_readonly(
      status=SessionStatus.ACTIVE,
      scheduled=False,
      include_running_status=True,
      include_pending_trigger_status=True,
      include_pending_plan_approval=True,
  )


async def _sessions_list_response(
    request: Request,
    rows: list[SessionMetadata],
    derived: dict[str, dict],
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
    memos: _SessionsListMemos,
) -> Response:
  """Render one sidebar list body and serve it from the route's own memos.

  The one render/memo body both sidebar routes share: each route selects its
  rows (the membership rule is the route's own) and hands them over with the
  listing's derived sidebar state; the helper projects the legacy worker-thread
  leaves, joins the schedule fields, renders the changed round, and stores
  every memo on *memos*. The readonly rows and the memoized leaves are
  identity-stable across requests, so the rendered body keys on the row
  identities plus the overlay states: a repeat of an unchanged corpus re-runs
  zero dumps (the search route's whole-body memo mechanism) and a reloaded
  meta or moved overlay state re-renders. The schedule join rides the same
  key, so a cron config change or a passing next-run re-renders the bound
  rows. A worker_thread row's fields are construction-fixed, so only the
  parent rows carry overlay state.
  """
  projected = await project_worker_threads(rows, cfg, thread_mgr)
  schedule_fields = row_schedule_fields((row.id for row in projected), datetime.now(UTC))
  rendered: list[tuple[SessionMetadata, tuple, tuple]] = []
  for row in projected:
    schedule_state = tuple(schedule_fields[row.id].items())
    if row.worker_thread is not None:
      rendered.append((row, (), schedule_state))
      continue
    entry = derived[row.id]
    thinking = thinking_state.busy_since(row.id)
    next_trigger = entry[sidebar_state.NEXT_TRIGGER_AT]
    # A worker row displays its newest Run's backend (the delegation's target
    # model), never the inherited creation value; every other row keeps its
    # own persisted backend. The in-memory read rides the overlay tuple so a
    # backend change re-renders the memoized body.
    display_backend = (run_backend(row.id) or row.backend) if row.profile == "worker" else row.backend
    # Both datetimes ride the model's JSON scheme (pydantic-core renders UTC as
    # Z); a hand-rolled isoformat() would emit +00:00 inside an all-Z row.
    rendered.append(
        (
            row, (
                _UTC_DATETIME_JSON.dump_python(thinking, mode="json") if thinking is not None else None,
                entry[sidebar_state.HAS_RUNNING_TASKS],
                entry[sidebar_state.HAS_PENDING_TRIGGER], entry[sidebar_state.PENDING_TRIGGER_COUNT],
                _UTC_DATETIME_JSON.dump_python(next_trigger, mode="json") if next_trigger is not None else None,
                entry[sidebar_state.HAS_PENDING_PLAN_APPROVAL], display_backend, entry.get(sidebar_state.WORK_STATE)),
            schedule_state))
  list_rows = tuple(row for row, _s, _sched in rendered)
  list_states = tuple((state, schedule_state) for _row, state, schedule_state in rendered)
  cached = memos.whole_body
  if (cached is not None and len(cached[0]) == len(list_rows) and
      all(c is r for c, r in zip(cached[0], list_rows, strict=True)) and cached[1] == list_states):
    return await gzip_body_response(request, cached[2], {}, memos.gzip_memo)
  payload = []
  for row, (state, schedule_state) in zip(list_rows, list_states, strict=True):
    rendered = memos.row_render.get(id(row))
    if rendered is not None and rendered[0] == state and rendered[1] == schedule_state:
      payload.append(rendered[3])
      continue
    dump = row.model_dump(mode="json", exclude=_RESPONSE_ROW_EXCLUDE)
    if state:
      (
          dump["thinking_since"], dump["has_running_tasks"], dump["has_pending_trigger"], dump["pending_trigger_count"],
          dump["next_trigger_at"], dump["has_pending_plan_approval"], dump["backend"], work_state) = state
      if work_state is not None:
        # A task-tree row carries its fact-derived work verdict — the same
        # derivation the /status payload serves — so the first paint shows the
        # running/waiting icons without a poll. A legacy row's key
        # set stays byte-identical (the dump already carries the field's null).
        dump[sidebar_state.WORK_STATE] = work_state
    payload.append(apply_row_schedule(dump, dict(schedule_state)))
    memos.row_render[id(row)] = (state, schedule_state, row, payload[-1])
  if len(memos.row_render) > len(list_rows):
    # A row that left the projection (archived, completed, filtered) holds a
    # slot nothing will ever consult again; drop it so the map stays at the
    # corpus the route serves.
    live = {id(row) for row in list_rows}
    for stale in [row_id for row_id in memos.row_render if row_id not in live]:
      del memos.row_render[stale]
  body = fast_json_bytes(payload)
  memos.whole_body = (list_rows, list_states, body)
  # The Response return skips response_model's jsonable_encoder pass over every
  # projected row; the body-keyed memo serves the middleware's deflate.
  return await gzip_body_response(request, body, {}, memos.gzip_memo)


@router.get("/")
async def list_sessions(
    request: Request,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> Response:
  """List active sessions newest first, each legacy row followed by its worker-leaf rows.

  Cron-subtree rows and the chat-thread subtree ride no listing: a firing leaf
  whose parent chain reaches a cron session stays out, and so does every
  Slack/Discord thread session with the descendants its ``task_parent_id``
  chains reach (the sidebar's Threads view lists that subtree through
  /chat-threads), so a parentless leaf never flattens into a top-level row.
  Every row's schedule fields come from the one join (row_schedule_fields).
  """
  rows, derived = await _active_listing_corpus(session_mgr)
  cron_subtree = await session_mgr.cron_subtree_roots()
  chat_threads = await session_mgr.chat_thread_subtree_roots()
  rows = [row for row in rows if row.id not in cron_subtree and row.id not in chat_threads]
  return await _sessions_list_response(request, rows, derived, cfg, thread_mgr, _workspace_list_memos)


@router.get("/chat-threads")
async def list_chat_threads(
    request: Request,
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> Response:
  """List the active chat-thread subtree newest first: the sidebar Threads view's rows.

  The complement of the Workspace root list over the same active corpus: the
  only rows kept are the Slack/Discord thread sessions — a session carrying a
  ``slack_origin`` or ``discord_origin`` — and every descendant their
  ``task_parent_id`` chains reach, the projected legacy worker-thread leaves
  included. Row shape, projection, schedule join, and render are the shared
  helper's; the render memos are this route's own, so the two lists never
  evict each other.
  """
  rows, derived = await _active_listing_corpus(session_mgr)
  chat_threads = await session_mgr.chat_thread_subtree_roots()
  rows = [row for row in rows if row.id in chat_threads]
  return await _sessions_list_response(request, rows, derived, cfg, thread_mgr, _chat_threads_list_memos)


@router.post("/", response_model=SessionMetadata)
async def create_session(
    req: CreateSessionRequest,
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> SessionMetadata:
  if any(getattr(req, f) is not None for f in ("request_id", "task_parent_id", "profile", "task")):
    # v2 task create: the task-tree owner binds (parent, request_id) to one node.
    if req.request_id is None:
      raise HTTPException(status_code=400, detail=TASK_CREATE_REQUEST_ID_REQUIRED)
    if req.backend is not None:
      _resolve_requested_backend(req.backend, cfg, fallback_backend=_default_backend_id(cfg))
    try:
      meta = await task_mgr.create_task(
          request_id=req.request_id,
          task_parent_id=req.task_parent_id,
          profile=req.profile,
          task=req.task,
          name=req.name,
          backend=req.backend,
          group=req.group,
          caller=caller,
      )
    except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError, DelegationBlockedError) as e:
      raise _task_http_error(e) from e
    log.info("task_created", session_id=meta.id, task_parent_id=req.task_parent_id, profile=req.profile)
    return meta
  if not caller.is_operator:
    # Run credentials create only their own child tasks under their own manager
    # node (the v2 path above); the legacy create shape is operator scope.
    raise HTTPException(status_code=403, detail=AGENT_CREATE_SCOPE_REFUSAL)
  backend = _resolve_requested_backend(req.backend, cfg, fallback_backend=_default_backend_id(cfg))
  log.info("creating_session", backend=backend, name=req.name)
  # The legacy create shape (the sidebar's new-session action) now opens a
  # manager root; each press is its own request, so it binds its own node.
  return await task_mgr.create_task(
      request_id=f"operator-create-{uuid.uuid4()}",
      task_parent_id=None,
      profile="manager",
      task=None,
      name=req.name,
      backend=backend,
      group=req.group,
      session_id=req.session_id,
      slack_origin=req.slack_origin,
      discord_origin=req.discord_origin,
      caller=caller)


class ArchivedGroupCount(BaseModel):
  group: str | None
  total: int


class ArchivedSessionsPage(BaseModel):
  sessions: list[dict]
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
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> dict:
  """One keyset page of archived sessions, newest first, with group aggregates for the filter strip.

  Page size, cursor, and the group aggregates count archived rows only. Each
  page also carries its rows' unarchived ancestors as ``context_only`` rows
  (the one walk on the manager, archived_context_rows), merged into the page's
  one newest-first list, so the client merges pages into a single
  project-grouped tree where a delivered firing nests under its still-active
  scheduled node without a second fetch.
  """
  try:
    page = await session_mgr.list_archived_page(group=group, limit=limit, before=before, before_id=before_id)
  except ValueError as e:
    raise HTTPException(status_code=422, detail=str(e)) from e
  rows = await project_worker_threads(page["sessions"], cfg, thread_mgr)
  # The projection above appends each legacy row's worker-thread leaves; under
  # an archived cron session those leaves are cron-subtree rows and stay out.
  # Every leaf's parent rides the page, so the membership walk classifies the
  # projected rows from the page's own rows.
  cron_subtree = cron_subtree_roots(rows)
  rows = [row for row in rows if row.id not in cron_subtree]
  context = await session_mgr.archived_context_rows(rows)
  # One page list, newest first: the stable sort keeps the archived rows in
  # their keyset order and interleaves the context rows by the same key.
  merged = sorted(
      [*((False, row) for row in rows), *((True, row) for row in context)],
      key=lambda pair: (pair[1].updated_at, pair[1].id),
      reverse=True)
  schedule_fields = row_schedule_fields((row.id for _context_only, row in merged), datetime.now(UTC))
  page["sessions"] = []
  for context_only, row in merged:
    dump = apply_row_schedule(row.model_dump(mode="json", exclude=_RESPONSE_ROW_EXCLUDE), schedule_fields[row.id])
    if context_only:
      dump["context_only"] = True
    page["sessions"].append(dump)
  return page


@router.get("/starred")
async def list_starred_sessions(
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> list[dict]:
  """List starred sessions, newest first, with their legacy worker-thread leaves.

  Row shape matches the other sidebar lists: the model dump with the schedule
  join's fields, so a starred row the client has also archived later renders
  the archived row form without a second endpoint.
  """
  sessions = await session_mgr.list_sessions(
      starred=True,
      include_running_status=True,
      include_pending_trigger_status=True,
  )
  projected = await project_worker_threads(sessions, cfg, thread_mgr)
  schedule_fields = row_schedule_fields((row.id for row in projected), datetime.now(UTC))
  return [
      apply_row_schedule(row.model_dump(mode="json", exclude=_RESPONSE_ROW_EXCLUDE), schedule_fields[row.id])
      for row in projected
  ]


@router.get("/groups")
async def list_groups(session_mgr: SessionManager = Depends(get_session_manager)) -> list[str]:
  """Return sorted distinct group names across all sessions."""
  return await session_mgr.list_group_names()


@router.post("/groups/rename")
async def rename_group(req: RenameGroupRequest, session_mgr: SessionManager = Depends(get_session_manager)) -> dict:
  """Rename a group across all sessions."""
  count = await session_mgr.rename_group(req.old_name, req.new_name)
  return {"updated": count}


@router.post("/groups/delete")
async def delete_group(req: DeleteGroupRequest, session_mgr: SessionManager = Depends(get_session_manager)) -> dict:
  """Remove a group from all sessions (sets group to null)."""
  count = await session_mgr.delete_group(req.group)
  return {"updated": count}


def _parse_session_ids(ids: str) -> list[str]:
  """Split the `ids` query parameter into a deduplicated, order-preserving id list."""
  # dict.fromkeys dedups in C while keeping first occurrence order; the empty
  # filter runs after it because a stripped-to-empty part is dropped either way.
  parsed = [sid for sid in dict.fromkeys(map(str.strip, ids.split(','))) if sid]
  if not parsed:
    raise HTTPException(status_code=422, detail="ids must name at least one session")
  return parsed


# The /status poll's whole-body memo: (requested ids, sidebar generation) -> the
# rendered body bytes. Every payload input sits behind the sidebar generation
# (mark_sidebar_dirty bumps it for busy flips, every metadata write through
# save_metadata's funnel, and every whole-session deletion through
# delete_session_permanently; store_snapshot_entry for probe stores) — the row
# set included — so an unchanged generation proves the stored body current and
# the hit path serves it without resolving the ids at all. The requested ids key the memo: the sidebar asks
# for exactly the rows it renders, and a row leaves the request set when the
# listing that feeds the sidebar refreshes. Keyed at the generation the
# request started at: a bump that lands mid-handler keys the next poll's
# rebuild, never this body. force=1 keeps its synchronous probe off this memo.
_STATUS_BODY_MEMO_LIMIT = 4
_status_body_memo: BoundedMemo[tuple, bytes] = BoundedMemo(_STATUS_BODY_MEMO_LIMIT)


@router.get('/status', response_model=None)
async def all_sessions_status(
    request: Request,
    ids: str = Query(..., description=_SIDEBAR_IDS_QUERY_DESC),
    force: bool = False,
    session_mgr: SessionManager = Depends(get_session_manager),
) -> Response:
  """Return derived sidebar state for the requested sessions.

  A poll at an unchanged sidebar generation serves the last rendered body
  whole, without resolving the requested ids. A memo miss resolves the ids and
  re-probes only the sessions whose probed state changed since the last poll.
  Pass ``force=1`` to skip the memo and the dirty check, re-probing every
  requested session; every 10th poll also schedules the detached single-flight
  self-heal sweep (its results land for the polls that follow it), while
  ``force=1`` keeps its probe synchronous and full.
  """
  requested = _parse_session_ids(ids)
  generation = sidebar_state.derived_generation()
  body_key = (tuple(requested), generation)
  if not force:
    cached_body = _status_body_memo.get(body_key)
    if cached_body is not None:
      if sidebar_state.register_poll(force=False):
        # The every-10th tick keeps its sweep: the sweep's probe builds from
        # the sessions' metadata, so the resolution the memo hit skipped
        # happens here, on the one poll in ten that carries the tick.
        sessions = await session_mgr.get_sessions_readonly(requested)
        active = [m for m in sessions if m.status != SessionStatus.ARCHIVED]
        if active:
          session_mgr.schedule_sidebar_sweep(active)
      return await gzip_body_response(request, cached_body, {}, _switch_gzip_memo)
  sessions = await session_mgr.get_sessions_readonly(requested)
  if not sessions:
    return await _switch_payload_response(request, {})
  derived = await session_mgr.resolve_sidebar_state(
      sessions,
      include_running_status=True,
      include_pending_trigger_status=True,
      include_pending_plan_approval=True,
      force=force,
  )
  result: dict[str, dict] = {}
  for meta in sessions:
    busy = thinking_state.busy_since(meta.id)
    entry = derived[meta.id]
    next_trigger_at = entry[sidebar_state.NEXT_TRIGGER_AT]
    payload = {
        "has_unread": bool(meta.has_unread),
        sidebar_state.HAS_RUNNING_TASKS: entry[sidebar_state.HAS_RUNNING_TASKS],
        "thinking_since": busy.isoformat() if busy else None,
        sidebar_state.HAS_PENDING_TRIGGER: entry[sidebar_state.HAS_PENDING_TRIGGER],
        sidebar_state.PENDING_TRIGGER_COUNT: entry[sidebar_state.PENDING_TRIGGER_COUNT],
        sidebar_state.NEXT_TRIGGER_AT: next_trigger_at.isoformat() if next_trigger_at else None,
        sidebar_state.HAS_PENDING_PLAN_APPROVAL: entry[sidebar_state.HAS_PENDING_PLAN_APPROVAL],
    }
    if sidebar_state.WORK_STATE in entry:
      # A task-tree row carries its fact-derived work verdict. A legacy row's
      # key set stays byte-identical to today.
      payload[sidebar_state.WORK_STATE] = entry[sidebar_state.WORK_STATE]
    result[meta.id] = payload
  body = fast_json_bytes(result)
  _status_body_memo.store(body_key, body)
  # The sidebar's 3 s poll is this host's second-busiest route; the gzip form
  # rides the body-keyed memo (_switch_payload_response's memo).
  return await gzip_body_response(request, body, {}, _switch_gzip_memo)


_SEARCH_ROW_FRAGMENT_CAP = 512
# The derived keys' wire prefixes, prebuilt once: the splice below joins
# them per row per request, so the prefix render is a dict hit instead of an
# encode plus two concats per key.
_SEARCH_DERIVED_PREFIXES: dict[str, bytes] = {
    "thinking_since": b'"thinking_since":',
    sidebar_state.HAS_RUNNING_TASKS: b'"has_running_tasks":',
    sidebar_state.HAS_PENDING_TRIGGER: b'"has_pending_trigger":',
    sidebar_state.PENDING_TRIGGER_COUNT: b'"pending_trigger_count":',
    sidebar_state.NEXT_TRIGGER_AT: b'"next_trigger_at":',
    sidebar_state.WORK_STATE: b'"work_state":',
}
_SEARCH_DERIVED_KEYS = frozenset(_SEARCH_DERIVED_PREFIXES)
# The two datetime fields are None for the common idle session; their whole
# ``"key":null`` piece is prebuilt so neither the pydantic dump_python call
# nor the scalar render ever runs for them. The other fields' types (bool, int,
# the work verdict's str-or-None) render through _json_scalar_bytes directly.
_SEARCH_NULL_PIECES: dict[str, bytes] = {
    key: prefix + b"null"
    for key, prefix in _SEARCH_DERIVED_PREFIXES.items()
    if key in ("thinking_since", sidebar_state.NEXT_TRIGGER_AT)
}
_search_row_fragments: BoundedMemo[int, tuple[SessionMetadata, tuple[bytes | str,
                                                                     ...]]] = BoundedMemo(_SEARCH_ROW_FRAGMENT_CAP)
# The finished row's wire bytes: metadata object -> (that object, the row's five
# derived values, the spliced body). The body is a pure function of the metadata
# object and those five values, both in the value — the metadata cache replaces
# the object whenever its file provably changes, and the values ride the check —
# so a repeat request with an unchanged row state serves the cached bytes; the
# stored object pins the row so an id reuse can never serve another object's
# bytes.
_SEARCH_ROW_BODY_CAP = 512
_search_row_bodies: BoundedMemo[int, tuple[SessionMetadata, tuple, bytes]] = BoundedMemo(_SEARCH_ROW_BODY_CAP)
# The whole response body of the last render: (the rows' metadata objects, each
# row's derived values, the body). The body is a pure function of the row
# sequence and those values, both in the check — so the steady-state repeat
# request (the debounced search box re-firing the same query) serves the cached
# bytes after one identity-and-values compare and rebuilds only when a row's
# state moved. The stored rows pin their objects, so ids in play can never name
# another object; the render runs synchronously on the event loop between
# awaits, so the single slot needs no lock.
_search_whole_body: tuple[tuple[SessionMetadata, ...], tuple, bytes] | None = None
# The body's gzip form rides the same pure-function ground as the plain bytes
# above, so the served shape (the browser's search fetch always sends
# Accept-Encoding: gzip) reads the stored compressed bytes instead of paying
# the gzip middleware's per-request deflate. Two slots cover the alternating
# queries a correction keystroke re-fires; each slot holds one ~32 KB wire body.
_SEARCH_GZIP_MEMO_LIMIT = 2
_search_gzip_memo: BoundedMemo[bytes, bytes] = BoundedMemo(_SEARCH_GZIP_MEMO_LIMIT)


def _search_row_static_segments(meta: SessionMetadata) -> tuple[bytes | str, ...]:
  """Return the row's dump segments: static JSON runs and derived key names.

  The route's overlay assigns its derived fields onto keys the model
  already declares, so dict assignment keeps them at their model-definition
  positions; the segments preserve that order — static runs as rendered JSON
  (no braces, rendered by :func:`fast_json_bytes`), each derived key as its
  name for the per-request value render.

  Memoized on the cached metadata object's identity, which the value pins with
  a strong reference so an id reuse can never serve another object's segments:
  the metadata cache replaces the object whenever its file provably changes
  (every writer publishes through the atomic tmp rename and the re-parse is a
  fresh instance), so identity is the same invalidation ground the read-only
  search's shared-reference contract stands on.
  """
  cached = _search_row_fragments.get(id(meta))
  if cached is not None and cached[0] is meta:
    return cached[1]
  row = meta.model_dump(mode="json", exclude=_RESPONSE_ROW_EXCLUDE)
  segments: list[bytes | str] = []
  static: dict = {}
  for key, value in row.items():
    if key in _SEARCH_DERIVED_KEYS:
      if static:
        segments.append(fast_json_bytes(static)[1:-1])
        static = {}
      segments.append(key)
    else:
      static[key] = value
  if static:
    segments.append(fast_json_bytes(static)[1:-1])
  _search_row_fragments.store(id(meta), (meta, tuple(segments)))
  return tuple(segments)


def _json_scalar_bytes(value: object) -> bytes:
  """Render one overlay value as JSON bytes.

  The overlay's value types are code-fixed (None, bool, int, and the ISO
  strings the two datetime slots carry); these renderings are byte-identical
  to the response render's own (orjson's) for those types, and an ISO 8601
  string is pure ASCII with no quote or backslash, so the quoted form needs no
  escape pass. Any other type fails loudly instead of rendering.
  """
  if value is None:
    return b"null"
  if value is True:
    return b"true"
  if value is False:
    return b"false"
  if isinstance(value, int):
    return b"%d" % value
  if isinstance(value, str):
    return b'"' + value.encode("ascii") + b'"'
  raise ValueError(f"unsupported overlay value type: {type(value).__name__}")


class SessionDetailResponse(SessionMetadata):
  """GET /api/sessions/{id} response: the session metadata plus the derived task fields."""
  task_state: TaskState = "open"
  work_state: WorkState = "idle"

  @field_validator("work_state", mode="before")
  @classmethod
  def _unprobed_verdict_stays_idle(cls, value: object) -> object:
    """A row whose sidebar state was never probed carries no verdict (the
    transient field is None); the response's work_state stays the literal
    'idle' it always was for those rows."""
    return "idle" if value is None else value

  archived: bool = False
  ancestors: list[AncestorRef] = []
  # The scope/source/current-rule facts the later Task/Context UI reads; body
  # content stays in the immutable prompt_bodies store.
  prompt_rules: dict = {}


class TreePageResponse(BaseModel):
  """GET /api/sessions/tree response."""
  items: list[SessionRow]
  next_cursor: str | None = None
  tree_revision: str


class PendingTaskInput(BaseModel):
  """One pending task input with the source/text facts the acknowledgement UI shows."""
  id: str
  type: str
  timestamp: datetime | None = None
  actor: str | None = None
  source_session_id: str | None = None
  from_session_name: str | None = None
  text: str = ""


class PendingTaskInputsResponse(BaseModel):
  """GET /api/sessions/{id}/task-inputs/pending response."""
  items: list[PendingTaskInput]


def _task_http_error(e: Exception) -> HTTPException:
  """Translate one task-tree domain error into its planned HTTP shape."""
  if isinstance(e, TaskArchivedError):
    # The archived refusal's sentence is the whole detail: the sender reads
    # exactly "task <id> is archived".
    return HTTPException(status_code=409, detail=str(e))
  if isinstance(e, (TaskInvalidError,)):
    return HTTPException(status_code=400, detail=str(e))
  if isinstance(e, (TaskNotFoundError, RunNotFoundError)):
    return HTTPException(status_code=404, detail=str(e))
  if isinstance(e, (TaskForbiddenError, DelegationBlockedError)):
    return HTTPException(status_code=403, detail=str(e))
  if isinstance(e, (TaskConflictError, RunIdentityConflictError)):
    blockers = getattr(e, "blockers", None)
    if blockers is None:
      blockers = [str(e)]
    return HTTPException(status_code=409, detail={"message": str(e), "blockers": blockers})
  raise e


@router.get("/tree", response_model=TreePageResponse)
async def get_session_tree(
    parent_id: str | None = Query(default=None),
    include_archived: bool = Query(default=False),
    limit: int = Query(default=100, ge=1, le=500),
    cursor: str | None = Query(default=None),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> dict:
  """One revision-bound page of the task tree; empty parent_id queries the roots."""
  try:
    return await task_mgr.tree_page(
        parent_id=parent_id or None,
        include_archived=include_archived,
        limit=limit,
        cursor=cursor,
    )
  except (TaskInvalidError, TaskNotFoundError, TaskConflictError) as e:
    raise _task_http_error(e) from e


@router.get('/search', response_model=list[SessionMetadata])
async def search_sessions(
    request: Request,
    q: str = '',
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> list[SessionMetadata] | Response:
  """Full-text search across session names and chat content."""
  if not q.strip():
    # A whitespace query never reaches the capped scan: the route serves the
    # active list from the shared cached references and rides the same
    # whole-body-memo render — the copy path's per-row model_copy plus the
    # response-model walk measured multi-ms on the projected fan-out.
    rows, derived = await session_mgr.list_sessions_readonly(
        status=SessionStatus.ACTIVE,
        include_running_status=True,
        include_pending_trigger_status=True,
    )
    return await _serve_search_rows(request, rows, derived, cfg, thread_mgr)
  # The capped name-match shape (a short query) is this route's slowest
  # request: the read-only search serves cache references and the response
  # renders through FastJsonResponse with the derived fields overlaid, the
  # same shape the /status poll took — the manager's per-row copy+populate
  # pass and the response-model walk both measured multi-ms on the 200-row cap.
  rows, derived = await session_mgr.search_sessions_readonly(
      q.strip(),
      include_running_status=True,
      include_pending_trigger_status=True,
  )
  return await _serve_search_rows(request, rows, derived, cfg, thread_mgr)


async def _serve_search_rows(
    request: Request,
    rows: list[SessionMetadata],
    derived: dict[str, dict],
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
) -> Response:
  """Render the search route's rows through the whole-body memo and serve.

  Both query shapes feed it — the capped name-match search and the
  whitespace query's active list — with rows as the shared cached
  references and ``derived`` mapping each row's id to the sidebar-state
  fields the response overlays.
  """
  global _search_whole_body
  # Each row's bytes splice the memoized static segments with the derived
  # values, rendered only when the row's state first produces a body (the whole
  # body serves the steady-state repeat, a churn round re-renders the moved
  # rows). orjson renders a dict context-free, so the spliced body is
  # byte-identical to the FastJsonResponse render of the merged dicts: the
  # segments follow the model's own key order, which is the order the in-place
  # overlay leaves the merged dicts in. Key prefixes ride the prebuilt bytes in
  # _SEARCH_DERIVED_PREFIXES, and a None datetime field rides its whole prebuilt
  # null piece (both fields are None on the common idle row), so the pydantic
  # dump_python call under it never runs.
  # A legacy row's projected worker-thread leaves ride directly after it with
  # their own state tuples: a leaf's only live fact is its thread's running
  # state (no thinking, no pending trigger), and the leaf memo keeps the row
  # objects identity-stable so the whole-body memo below still serves.
  rendered: list[tuple[SessionMetadata, tuple]] = []
  for meta in rows:
    entry = derived[meta.id]
    rendered.append(
        (
            meta, (
                thinking_state.busy_since(meta.id), entry[sidebar_state.HAS_RUNNING_TASKS],
                entry[sidebar_state.HAS_PENDING_TRIGGER], entry[sidebar_state.PENDING_TRIGGER_COUNT],
                entry[sidebar_state.NEXT_TRIGGER_AT], entry.get(sidebar_state.WORK_STATE))))
    if meta.profile is not None:
      continue
    for thread_row in await view_thread_rows(meta.id, cfg, thread_mgr):
      leaf = _projected_thread_row(meta, thread_row)
      rendered.append((leaf, (None, leaf.has_running_tasks, False, 0, None, None)))
  search_rows = tuple(m for m, _s in rendered)
  search_states = tuple(s for _m, s in rendered)
  cached = _search_whole_body
  if (cached is not None and len(cached[0]) == len(search_rows) and
      all(c is m for c, m in zip(cached[0], search_rows, strict=True)) and cached[1] == search_states):
    return await gzip_body_response(request, cached[2], {}, _search_gzip_memo)
  parts: list[bytes] = []
  for meta, (thinking_since, has_running, has_pending, pending_count, next_trigger_at, work_state) in \
          zip(search_rows, search_states, strict=True):
    row_key = (thinking_since, has_running, has_pending, pending_count, next_trigger_at, work_state)
    body = _search_row_body(meta, row_key)
    parts.append(body)
  body = b"[" + b",".join(parts) + b"]"
  _search_whole_body = (search_rows, search_states, body)
  return await gzip_body_response(request, body, {}, _search_gzip_memo)


def _search_row_body(meta: SessionMetadata, row_key: tuple) -> bytes:
  """Render one search row's wire bytes, memoized on the row's identity and state."""
  cached = _search_row_bodies.get(id(meta))
  if cached is not None and cached[0] is meta and cached[1] == row_key:
    return cached[2]
  values = {
      "thinking_since": (_UTC_DATETIME_JSON.dump_python(row_key[0], mode="json") if row_key[0] is not None else None),
      sidebar_state.HAS_RUNNING_TASKS: row_key[1],
      sidebar_state.HAS_PENDING_TRIGGER: row_key[2],
      sidebar_state.PENDING_TRIGGER_COUNT: row_key[3],
      sidebar_state.NEXT_TRIGGER_AT:
          (_UTC_DATETIME_JSON.dump_python(row_key[4], mode="json") if row_key[4] is not None else None),
      sidebar_state.WORK_STATE: row_key[5],
  }
  rendered: list[bytes] = []
  for segment in _search_row_static_segments(meta):
    if isinstance(segment, bytes):
      rendered.append(segment)
      continue
    value = values[segment]
    null_piece = _SEARCH_NULL_PIECES.get(segment)
    if null_piece is not None and value is None:
      rendered.append(null_piece)
    else:
      rendered.append(_SEARCH_DERIVED_PREFIXES[segment] + _json_scalar_bytes(value))
  body = b"{" + b",".join(rendered) + b"}"
  _search_row_bodies.store(id(meta), (meta, row_key, body))
  return body


# The switch fetch (bootstrap) and the sidebar's status poll (/status)
# rebuild their payload per request, so unlike the
# events page there is no projection generation to key a gzip form on; the
# rendered body bytes are their own invalidation ground — a memo hit proves
# byte equality because the dict key IS the body. Without it every
# gzip-accepting fetch pays the middleware's whole-body level-1 deflate in the
# send path, the M35 events-page cost the projection fix removed there.
# Content-Encoding set upstream is what makes that middleware skip its own
# pass (the M72 listing mechanism). The limit covers one steady-state body per
# open tab's id set plus the other callers'.
_SWITCH_GZIP_MEMO_LIMIT = 16
_switch_gzip_memo: BoundedMemo[bytes, bytes] = BoundedMemo(_SWITCH_GZIP_MEMO_LIMIT)


async def _switch_payload_response(request: Request, payload: dict | list) -> Response:
  """Render a request-path payload once and serve its gzip form from the body-keyed memo."""
  return await gzip_body_response(request, fast_json_bytes(payload), {}, _switch_gzip_memo)


@router.get('/{session_id}/pending-triggers')
async def get_pending_triggers(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    trigger_mgr: TriggerManager = Depends(get_trigger_manager),
) -> list[dict]:
  """Return the session's pending delayed triggers, fire_at ascending.

  The chat column's pending-triggers tray is the one consumer: each element is
  the trigger record's JSON form (PendingTrigger dumped with mode="json"), so
  the tray renders the watch targets and fire_at the record carries. Fired and
  cancelled records stay out — the transcript shows fired triggers, and the
  cancel endpoint (src/runtime/api/internal.py) owns removal.
  """
  triggers = await trigger_mgr.list_triggers(session_id)
  pending = sorted(
      (tr for tr in triggers if tr.status is TriggerStatus.PENDING),
      key=lambda tr: tr.fire_at,
  )
  return [tr.model_dump(mode="json") for tr in pending]


@router.get('/{session_id}/bootstrap')
async def get_session_bootstrap(
    session_id: str,
    request: Request,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> Response:
  """Return the minimal data needed to make one chat session usable."""
  bootstrap = await build_session_bootstrap_data(session_id, session_mgr, tree=task_mgr)
  # The switch fetch's gzip form rides the body-keyed memo (_switch_payload_response).
  return await _switch_payload_response(request, _bootstrap_payload(bootstrap, cfg))


@router.post('/{session_id}/read')
async def mark_session_read(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> dict:
  """Clear the session's unread flag; the client posts this after a render lands.

  "Read" means "content rendered": the bootstrap GET stays side-effect-free
  and this explicit POST is the only flip-off path, so a bare data fetch can no
  longer wipe the sidebar's unread dot. Flip semantics and the unread_changed
  broadcast (only on an actual flip) are SessionManager.mark_read's own.
  """
  await session_mgr.mark_read(session_id)
  return {"session_id": session_id, "has_unread": False}


@router.get('/{session_id}/usage')
async def get_session_usage(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> FastJsonResponse:
  """Return lazy session status and usage data for the active header."""
  if meta.profile == "worker":
    from src.runtime.api.message_utils import _worker_usage
    usage = await _worker_usage(task_mgr, session_id)
  else:
    usage = await session_mgr.resolve_session_usage(session_id, meta)
  payload = {
      "session": meta.model_dump(mode="json", exclude=_RESPONSE_ROW_EXCLUDE),
      "usage": usage,
  }
  if meta.profile == "worker":
    from src.runtime import worker_transcript
    entry = await asyncio.to_thread(worker_transcript.load_worker_transcript, task_mgr, session_id)
    payload["active_run_id"] = entry.active_run_id
  payload.update(_active_backend_payload(meta, cfg))
  return FastJsonResponse(payload)


@router.get('/{session_id}/events')
async def get_session_events_page(
    session_id: str,
    request: Request,
    before: int,
    limit: int = 40,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> Response:
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
  if meta.profile == "worker":
    # A worker node's messages are its Runs' transcript; the same turn-aligned
    # page contract, sliced off the transcript projection (message ordinals in
    # the transcript's own cursor space).
    from src.runtime import worker_transcript
    entry = await asyncio.to_thread(worker_transcript.load_worker_transcript, task_mgr, session_id)
    messages, next_before, has_more = entry.projection.slice_before(before, limit)
    return FastJsonResponse({"messages": messages, "has_more": has_more, "next_before": next_before})
  # FastJsonResponse skips the jsonable_encoder pass FastAPI runs on mapped
  # returns; on this payload (211 messages / 559 KB) that pass measures ~3x a
  # plain json.dumps, and every field is already a plain parsed-JSON type so
  # the dumped body is unchanged.
  if meta.archive_offset == 0:
    projection = await get_message_projection_fast(session_mgr, session_id)
    if projection is not None:
      # The chat UI re-fetches a page whenever it re-enters the viewport or the
      # session is revisited, and the published projection is immutable, so a
      # repeat page serves its rendered body from the projection's own cache;
      # every advance publishes a new projection whose cache starts empty.
      body = projection.cached_page_body(before, limit)
      if body is None:
        messages, next_before, has_more = projection.slice_before(before, limit)
        body = fast_json_bytes({"messages": messages, "has_more": has_more, "next_before": next_before})
        projection.store_page_body(before, limit, body)
      if request_wants_gzip(request):
        gz = projection.cached_page_body_gzip(before, limit)
        if gz is None:
          # One deflate per page per projection generation, in the executor the
          # middleware's replaced pass also used.
          gz = await asyncio.to_thread(gzip_level1, body)
          projection.store_page_body_gzip(before, limit, gz)
        return PreencodedJSONResponse(gz, headers=GZIP_RESPONSE_HEADERS)
      return PreencodedJSONResponse(body)
  start = max(0, before - limit)
  events, has_more = await asyncio.to_thread(session_mgr.load_chat_events_range, session_id, start, before)
  messages = events_to_messages(events, event_index_offset=start)
  return FastJsonResponse({"messages": messages, "has_more": has_more, "next_before": start})


@router.get('/{session_id}/transcript')
async def get_session_transcript(
    session_id: str,
    after: int = Query(default=0, ge=0, description="Rendered message count (the client's cursor)"),
    revision: str = Query(default='', description="The revision the client last rendered"),
    thread: str | None = Query(default=None, description="Legacy thread id (thread view)"),
    meta: SessionMetadata = Depends(require_session),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
) -> FastJsonResponse:
  """The live-transcript poll for a worker node or a legacy thread view.

  The open chat asks every ~2 s while the view shows one of them. An unchanged
  transcript costs one stat pass; appended messages ride the append-only
  committed list past *after*. *revision* moves exactly when the rendered
  prefix is no longer current (a Run's state line changed, the delivery close
  appeared), and the response then carries the full history with ``reset``
  set, so the client re-renders instead of appending.
  """
  from src.runtime import worker_transcript
  if thread is not None:
    thread_meta = await thread_mgr.get_thread(session_id, thread)
    if thread_meta is None:
      raise HTTPException(status_code=404, detail=f"thread {thread} not found in session {session_id}")
    entry = await asyncio.to_thread(
        worker_transcript.load_thread_transcript, cfg, cfg.sessions_dir / session_id, thread_meta, await
        thread_mgr.get_events_log_path(session_id, thread))
    thinking_since = worker_transcript.thread_thinking_since(thread_meta)
  else:
    if meta.profile != "worker":
      raise HTTPException(status_code=400, detail=f"session {session_id} has no worker transcript")
    entry = await asyncio.to_thread(worker_transcript.load_worker_transcript, task_mgr, session_id)
    thinking_since = thinking_state.busy_since(session_id)
  # Both transcript arms answer one frontend poll (web/static/js/sidebar/session-view.js),
  # so the response body lives here once: a field added to one arm only would
  # silently drop from the other view.
  reset = _transcript_reset(entry.revision, revision)
  return FastJsonResponse(
      {
          "messages": entry.projection.committed if reset else entry.projection.committed[after:],
          "total": len(entry.projection.committed),
          "pending_draft": entry.projection.pending_draft,
          "revision": entry.revision,
          "reset": reset,
          "active_run_id": entry.active_run_id,
          "thinking_since": thinking_since.isoformat() if thinking_since else None,
      })


def _transcript_reset(latest_revision: str, client_revision: str) -> bool:
  """True when the client's rendered prefix is no longer current."""
  return not client_revision or client_revision != latest_revision


@router.get('/{session_id}/recap')
async def get_session_recap(
    session_id: str,
    upto: int | None = None,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> FastJsonResponse:
  """Pure-extraction recap (no LLM) plus any cached Haiku summary for a divider.

  ``upto`` is a global event_index (default: latest). Returns ordered asks, the
  last exchange, the cached summary (or null), and whether that summary is stale.
  """
  from src.features.recap import recap
  if upto is None:
    count = await asyncio.to_thread(session_mgr.get_chat_event_count_sync, session_id)
    upto = max(0, count - 1)
  # The chat UI re-requests an open recap panel on every re-materialization, so a
  # repeat read answers from the extract + summary-cache memos on the event loop;
  # the executor round-trips are paid only on a memo miss.
  extract = recap.extract_recap_memo_hit(session_id, upto)
  if extract is None:
    extract = await asyncio.to_thread(recap.extract_recap, session_mgr, session_id, upto)
  summary = recap.summary_lookup_memo_hit(session_mgr, session_id, upto)
  if summary is None:
    summary = await asyncio.to_thread(recap.lookup_cached_summary, session_mgr, session_id, upto)
  summary_text, stale = summary
  return FastJsonResponse({**extract, "summary": summary_text, "summary_stale": stale})


@router.post('/{session_id}/recap/summarize')
async def summarize_session_recap(
    session_id: str,
    upto: int,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """Generate (via a light backend), cache, and return the recap summary for a divider."""
  from src.features.recap import recap
  summary = await recap.generate_and_cache_summary(session_mgr, session_id, upto, cfg)
  return {"summary": summary}


@router.post('/{session_id}/explain')
async def request_session_explain(
    session_id: str,
    body: ExplainRequest,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> FastJsonResponse:
  """Register (or return) the explain task for a divider; generation runs detached.

  A divider with no entry, or a terminal one (re-run overwrites), returns 202 with
  the fresh pending entry; a pending one returns 200 with the stored entry so one
  divider never runs a second concurrent generation.
  """
  from src.features.explain import explain
  option = cfg.get_backend_option(body.backend)
  if option is None:
    raise bad_request(ValueError(f"unknown backend: {body.backend}"))
  entry, created = await explain.request_explain(session_mgr, session_id, body.event_index, option, cfg)
  return FastJsonResponse(entry, status_code=202 if created else 200)


@router.get('/{session_id}/explain')
async def get_session_explain(
    session_id: str,
    upto: int,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> FastJsonResponse:
  """The single explain entry for a divider; answer and error bodies included."""
  from src.features.explain import explain
  entry = await explain.get_explain_entry(session_mgr, session_id, upto)
  if entry is None:
    raise HTTPException(status_code=404, detail=f"no explain entry for event_index {upto}")
  return FastJsonResponse(entry)


@router.get('/{session_id}/explain/status')
async def get_session_explain_status(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> FastJsonResponse:
  """Every explain entry's ``{upto: {state, backend, generated_at}}`` summary; bodies excluded.

  The chat page pulls this once per session load/switch to render each divider's
  explain button from persisted truth.
  """
  from src.features.explain import explain
  return FastJsonResponse(await explain.explain_status(session_mgr, session_id))


async def _start_successor_run(
    task_mgr: TaskTreeManager,
    caller: CallerIdentity,
    meta: SessionMetadata,
    prompt_head: str,
    directive: str,
) -> None:
  """Admit a fork/elone successor's bootstrap prompt as its first input and dispatch the run.

  The prompt is built around the session's own chat log: *prompt_head* carries the opener plus
  whatever successor-specific context precedes the history note; *directive* tells the successor
  what to do with the copied history. The parts join into one single-spaced paragraph — the
  child's first input, admitted the way the message route admits a caller's input.
  """
  bootstrap_prompt = f"{prompt_head}{HISTORY_LOCATION_NOTE} {directive}"
  event_type = input_event_type_for_caller(caller)
  from_session, from_session_name = agent_provenance(caller) if event_type != ET.USER else (None, None)
  await task_mgr.dispatch.admit_input(
      meta.id,
      event_type=event_type,
      content=bootstrap_prompt,
      actor="user" if event_type == ET.USER else "agent",
      from_session=from_session,
      from_session_name=from_session_name,
  )
  await task_mgr.dispatch.dispatch_pending(meta.id)


@router.post('/{session_id}/fork', response_model=SessionMetadata)
async def fork_session(
    session_id: str,
    body: ForkSessionRequest | None = None,
    parent: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> SessionMetadata:
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
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL) from e
  except ValueError as e:
    raise bad_request(e) from e

  await _start_successor_run(
      task_mgr,
      caller,
      meta,
      prompt_head=f"{FORK_BOOTSTRAP_OPENER} ",
      directive="Get oriented from that log, summarize where things stand, and wait for the user's next instruction.")

  return meta


@router.post('/{session_id}/elone', response_model=SessionMetadata)
async def elone_session(
    session_id: str,
    body: EloneSessionRequest,
    parent: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> SessionMetadata:
  """Create an Elon-e session: fresh start with a bootstrap prompt that reads the parent."""
  backend = _resolve_requested_backend(body.backend, cfg, fallback_backend=parent.backend)
  try:
    meta = await session_mgr.elone_session(session_id, body.event_index, backend=backend)
  except FileNotFoundError as e:
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL) from e
  except ValueError as e:
    raise bad_request(e) from e

  await _start_successor_run(
      task_mgr,
      caller,
      meta,
      prompt_head=(
          f"{ELONE_BOOTSTRAP_OPENER} "
          "The dissatisfaction is usually with the most recent exchange before the takeover point. "),
      directive=(
          "Understand what the user wanted and where it went wrong, then give your read and a better approach. "
          "Confirm with the user before acting."))

  return meta


@router.post("/{session_id}/backend", response_model=SessionMetadata)
async def switch_session_backend(
    session_id: str,
    body: SwitchBackendRequest,
    parent: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> SessionMetadata:
  """Switch a session's backend in place, across model families.

  ``meta.backend`` is an effective current backend: the raw field when set,
  else ``backends.options[0]``. Every session accepts any configured backend
  id: the target backend starts its own native conversation when it cannot
  continue the held one (``claude_accounts.same_continuation_domain`` judges
  that at turn start), and catches up from the session's chat log. A
  cron-dedicated session keeps the in-domain restriction — the scheduler
  re-aligns it to its task config on every trigger, so its backend is decided
  by that config, and an out-of-domain target is refused.

  A session from before the native_backend rule (a held id with no recorded
  producer) is backfilled here, before the backend field changes, so the next
  turn judges the id against the backend that actually produced it.
  """
  valid_ids = {opt.id for opt in cfg.backends.options}
  if body.backend not in valid_ids:
    raise HTTPException(status_code=400, detail=_UNKNOWN_BACKEND_DETAIL.format(body.backend, sorted(valid_ids)))

  effective_current = parent.backend or _default_backend_id(cfg)
  if body.backend == effective_current:
    return parent

  from src.features.cron.cron_sequence import bound_task_name
  bound_task = bound_task_name(parent.id)
  if bound_task is not None and not claude_accounts.same_continuation_domain(effective_current, body.backend, cfg):
    raise HTTPException(
        status_code=400,
        detail=(
            f"backend '{body.backend}' cannot be switched to in place: this session is the bound node "
            f"of scheduled task '{bound_task}', whose cron config decides its backend and whose "
            "scheduler re-aligns the node to that config on every tick. Edit the task's config in the "
            "cron editor, or clone/fork the session with the target backend instead."),
    )

  # The pre-rule backfill: record the effective current backend as the held
  # id's producer through the authorized anchor channel, before the backend
  # field changes. Same-domain switches backfill too, so the next same-domain
  # turn still resumes as today.
  if parent.cc_session_id and not parent.native_backend:
    await session_mgr.persist_native_backend(session_id, effective_current)

  previous = effective_current
  meta = require_found(await session_mgr.switch_backend(session_id, body.backend))

  # The audit event is the switch history; the durable metadata read here (the
  # backfill included) is what it records. previous_native_backend is None when
  # the session holds no native id to attribute.
  durable = await session_mgr.read_metadata_fresh(session_id)
  audit_event = {
      "type": BACKEND_SWITCHED,
      "from": previous,
      "to": body.backend,
      "previous_native_backend": durable.native_backend if durable is not None and durable.cc_session_id else None,
      "previous_native_session_id": durable.cc_session_id if durable is not None else None,
  }
  await session_mgr.persist_and_broadcast(session_id, audit_event)
  return meta


@router.get("/{session_id}", response_model=SessionDetailResponse)
async def get_session(
    meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> SessionDetailResponse:
  """Session detail; task-tree nodes carry the derived task fields from one projection owner."""
  if meta.profile is None:
    return SessionDetailResponse(**meta.model_dump())
  try:
    detail = await task_mgr.session_detail(meta.id)
  except (TaskInvalidError, TaskNotFoundError, TaskConflictError) as e:
    raise _task_http_error(e) from e
  return SessionDetailResponse.model_validate(detail)


@router.delete("/{session_id}")
async def archive_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> Response:
  """The user's archive: on a task node the single end state, on a legacy
  session the stored-status archive.

  A task node's archive ends its whole subtree: every open node gets one
  ``task_closed`` fact with outcome ``archived`` (operator scope required; an
  unfinished run anywhere in the subtree refuses with 409 before anything is
  written). The response names the ids this call archived — an already
  archived node returns an empty list. A session without a profile keeps the
  legacy path: the empty-session delete, else the stored
  ``status: archived`` write.
  """
  if meta.profile is not None:
    if not caller.is_operator:
      raise HTTPException(status_code=403, detail="archiving a task requires operator credentials")
    try:
      archived = await task_mgr.archive_subtree(session_id, caller=caller)
    except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
      raise _task_http_error(e) from e
    return JSONResponse({"archived": archived})
  event_count = await asyncio.to_thread(session_mgr.get_chat_event_count_sync, session_id, meta)
  if event_count == 0:
    await session_mgr.delete_session_permanently(session_id)
    return meta

  return require_found(await session_mgr.archive_session(session_id))


@router.delete("/{session_id}/permanent", status_code=204)
async def delete_session_permanently(
    session_id: str,
    session_mgr: SessionManager = Depends(get_session_manager),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> Response:
  """Permanent delete: the empty/unreferenced rule is checked and deleted under
  the one control lock (v2 nodes); legacy sessions keep the v1 check set."""
  meta = await session_mgr.get_session(session_id)
  if meta is not None and meta.profile is not None:
    try:
      deleted = await task_mgr.delete_permanently(session_id, caller=caller)
    except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
      raise _task_http_error(e) from e
    if not deleted:
      raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL)
    return Response(status_code=204)
  try:
    blockers = await task_mgr.deletion_blockers(session_id)
  except (TaskInvalidError, TaskNotFoundError, TaskConflictError) as e:
    raise _task_http_error(e) from e
  if blockers:
    raise HTTPException(status_code=409, detail={"message": "permanent delete blocked", "blockers": blockers})
  result = await session_mgr.delete_session_permanently(session_id)
  if not result:
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL)
  return Response(status_code=204)


@router.post("/{session_id}/unarchive")
async def unarchive_session(
    session_id: str,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> Response:
  """Restore an archived node.

  On a task node this is the end state's only exit: the target and every
  archived ancestor restore (one ``task_reopened`` fact each, topmost first;
  siblings and descendants untouched; no round starts), operator scope
  required, and the response names the restored ids. A session without a
  profile keeps the legacy stored-status restore.
  """
  if meta.profile is not None:
    if not caller.is_operator:
      raise HTTPException(status_code=403, detail="unarchiving a task requires operator credentials")
    try:
      restored = await task_mgr.completion.restore_task(
          session_id, request_id=str(uuid.uuid4()), reason="sidebar unarchive", caller=caller)
    except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
      raise _task_http_error(e) from e
    return JSONResponse({"restored": restored["restored"]})
  if meta.status != SessionStatus.ARCHIVED:
    raise HTTPException(status_code=409, detail="Session is not archived")
  return require_found(await session_mgr.unarchive_session(session_id))


@router.post("/{session_id}/star", response_model=SessionMetadata)
async def star_session(session_id: str, session_mgr: SessionManager = Depends(get_session_manager)) -> SessionMetadata:
  return require_found(await session_mgr.star_session(session_id))


@router.post("/{session_id}/unstar", response_model=SessionMetadata)
async def unstar_session(
    session_id: str, session_mgr: SessionManager = Depends(get_session_manager)) -> SessionMetadata:
  return require_found(await session_mgr.unstar_session(session_id))


@router.post("/{session_id}/rounds/{round_id}/rate", response_model=SessionMetadata)
async def rate_round(
    session_id: str,
    round_id: str,
    req: RateRoundRequest,
    meta: SessionMetadata = Depends(require_session),
    session_mgr: SessionManager = Depends(get_session_manager),
) -> SessionMetadata:
  if req.rating is None:
    meta.round_ratings.pop(round_id, None)
  else:
    meta.round_ratings[round_id] = req.rating
  meta.updated_at = datetime.now(UTC)
  await session_mgr.save_metadata(meta)
  log.info("round_rated", session_id=session_id, round_id=round_id, rating=req.rating)
  return meta


@router.patch("/{session_id}", response_model=SessionDetailResponse)
async def patch_session(
    session_id: str,
    req: PatchSessionTaskRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> SessionDetailResponse:
  """Rename plus the v2 task metadata mutations (one PATCH body).

  A name-only PATCH keeps the legacy rename path; task fields route through
  the task-tree owner with their structural guards.
  """
  if req.model_fields_set <= {"name"}:
    if not req.name:
      raise HTTPException(status_code=400, detail="rename requires a non-empty name")
    meta = require_found(await session_mgr.rename_session(session_id, req.name))
    if meta.profile:
      # Task-tree nodes keep the tree projection and the sidebar in the same
      # loop as every other task mutation; legacy (non-tree) sessions have no
      # tree to refresh.
      task_mgr.invalidate_tree_index()
      await task_mgr.events.notify_tree_changed(session_id, "task_updated")
    return SessionDetailResponse(**meta.model_dump())
  try:
    meta = await task_mgr.patch_task(session_id, req, caller=caller)
  except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
    raise _task_http_error(e) from e
  return SessionDetailResponse.model_validate(await task_mgr.session_detail(meta.id))


@router.post("/{session_id}/group", response_model=SessionMetadata)
async def set_session_group(
    session_id: str,
    req: SetGroupRequest,
    session_mgr: SessionManager = Depends(get_session_manager),
) -> SessionMetadata:
  return require_found(await session_mgr.set_group(session_id, req.group))


# The raw events download's compressed serve memo: entries key on the file path
# and StatSignatureMemo checks the (mtime_ns, size) the read served, and the
# limit holds one big session's wire form per open events-viewer tab — a second
# flipped-to session misses once and re-compresses.
_EVENTS_GZIP_MEMO_LIMIT = 2
_events_gzip_memo: StatSignatureMemo[Path, bytes] = StatSignatureMemo(_EVENTS_GZIP_MEMO_LIMIT)


@router.get("/{session_id}/events.jsonl")
async def get_events_jsonl(session_id: str, request: Request) -> Response:
  """Serve the raw chat_events.jsonl file for a session."""
  cfg = get_config()
  path = chat_events_path(cfg.sessions_dir / session_id)
  if not path.exists():
    raise HTTPException(status_code=404, detail="Events file not found")
  if not request_wants_gzip(request):
    return FileResponse(path, media_type="application/x-ndjson")
  # The read and the deflate ride one executor hop: FileResponse streams 64 KiB
  # chunks and the gzip middleware compresses every chunk inline on the event
  # loop (the M101 loop-lag readings).
  body = await asyncio.to_thread(gzip_file_fresh, _events_gzip_memo, path, None)
  return Response(content=body, media_type="application/x-ndjson", headers=GZIP_RESPONSE_HEADERS)


@router.get("/{session_id}/threads", response_model=list[ThreadMetadata])
async def list_threads(
    session_id: str, thread_mgr: ThreadManager = Depends(get_thread_manager)) -> list[ThreadMetadata]:
  return await thread_mgr.list_threads(session_id)


@router.get("/{session_id}/plans")
async def list_plans(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    plan_mgr: PlanRegistryManager = Depends(get_plan_manager),
) -> FastJsonResponse:
  """Return the plan registry for a session with derived states and read errors.

  Unknown session → 404. Known session → always 200 with ``{"plans": [...], "errors": [...]}``;
  a corrupt registry produces 200 with empty plans and one error entry, never 5xx.
  """
  # The plan panel polls this route.
  return FastJsonResponse(await plan_mgr.list_plans(session_id))


# ---------------------------------------------------------------------------
# Task-tree run routes (schema_version=2)
# ---------------------------------------------------------------------------


@router.get("/{session_id}/runs", response_model=RunPage)
async def list_session_runs(
    session_id: str,
    limit: int = Query(default=100, ge=1, le=500),
    cursor: str | None = Query(default=None),
    order: str = Query(default="asc"),
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> RunPage:
  """One keyset page of the session's run records over the canonical launch order.

  ``order`` defaults to ``asc`` — chronological, queued reservations first —
  which keeps every existing client's pagination exactly as it was.
  ``order=desc`` reads the same (started_at, id) total order backwards: the
  most recently started run opens the page, so ``order=desc&limit=1`` is the
  session's authoritative latest launch without paging through older history.
  A queued reservation never opens a descending page while any run has
  started, and a cursor minted under one order is an explicit 400 under the
  other.

  Each row carries its fact-derived display state and stop-request flag — the
  same fold the guards consume, so a client never guesses state from a status
  badge.
  """
  if order not in ("asc", "desc"):
    raise HTTPException(status_code=400, detail=f"unknown runs order: {order!r} (use 'asc' or 'desc')")
  try:
    slice_ = await asyncio.to_thread(
        task_mgr.runs.list_runs_page_sync, session_id, limit, cursor, descending=(order == "desc"))
  except ValueError as e:
    raise bad_request(e) from e
  events = task_mgr.runs.load_events_sync(session_id)
  from src.runtime.runs import read_host_boot_time
  host_boot = await asyncio.to_thread(read_host_boot_time)
  rows = [
      RunRow(
          **run.model_dump(),
          state=task_mgr.runs.run_display_state(run, events, host_boot),
          stop_requested=task_mgr.runs.stop_requested(events, run.id),
      ) for run in slice_.items
  ]
  return RunPage(items=rows, next_cursor=slice_.next_cursor)


@router.get("/{session_id}/effective-prompt")
async def get_effective_prompt(
    session_id: str,
    kind: str | None = Query(default=None),
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> dict:
  """The next-start snapshot-shaped preview: one assembly path with the launch.

  ``kind`` selects the Run kind and defaults from the profile (manager_turn for
  a manager, work for a worker); an unknown kind is an explicit 400. A queued
  preview is the current configuration; after launch the historical endpoint
  (``/runs/{run_id}/context``) is authoritative. Read-only: never launches a
  process, mutates a native anchor, or manufactures inputs.
  """
  meta = await _require_task_meta(task_mgr, session_id)
  default_kind = "manager_turn" if meta.profile == "manager" else "work"
  resolved_kind = kind or default_kind
  if resolved_kind not in _RUN_KINDS:
    raise HTTPException(status_code=400, detail=f"unknown run kind: {kind!r}")
  # The preview must resolve the backend exactly as a launch would
  # (_resolve_session_default_backend_model): a task without its own backend
  # previews with the configured default option; a stale pinned id is refused.
  option = cfg.get_backend_option(meta.backend) if meta.backend else None
  if option is None:
    if meta.backend:
      raise HTTPException(
          status_code=400,
          detail=(f"session backend {meta.backend!r} is not configured; "
                  "update the task backend before previewing"))
    if not cfg.backends.options:
      raise HTTPException(status_code=400, detail=EMPTY_BACKENDS_OPTIONS_REFUSAL)
    option = cfg.backends.options[0]
  # The M99 import floor carries no launch-snapshot stack; the assembly rides the
  # endpoints that render it.
  from src.runtime.task_execution import assemble_coherent_snapshot
  from src.runtime.task_prompts import TaskPromptError
  try:
    snapshot, overlay_error, declared = await assemble_coherent_snapshot(cfg, task_mgr, meta, resolved_kind, option)
  except TaskPromptError as e:
    raise HTTPException(status_code=500, detail=str(e)) from e
  payload = {
      "session_id": session_id,
      "kind": resolved_kind,
      "mode": "preview",
      "overlay": {
          "declared": declared,
          "error": type(overlay_error).__name__ if overlay_error is not None else None,
      },
  }
  payload.update(snapshot.to_json_dict())
  return payload


async def _require_task_meta(task_mgr: TaskTreeManager, session_id: str) -> SessionMetadata:
  """The task metadata a context read needs, with the task-tree 404/400 mapping."""
  meta = await task_mgr.load_meta(session_id)
  if meta is None:
    raise HTTPException(status_code=404, detail=SESSION_NOT_FOUND_DETAIL)
  if meta.profile is None:
    raise HTTPException(status_code=400, detail=not_task_node_detail(session_id))
  return meta


def _legacy_prompt_payload(path: Path) -> dict:
  """The legacy_prompt payload for one recorded raw launch-text file: ref, content hash, note."""
  return {
      "ref":
          str(path),
      "sha256":
          sha256_hex(path.read_text(encoding="utf-8")),
      "note":
          (
              "raw launch text recorded before the context stage: the managed "
              "instructions are visible but per-source provenance was not "
              "recorded, so this is limited evidence, not a full snapshot"),
  }


@router.get("/{session_id}/runs/{run_id}/context")
async def get_run_context(
    session_id: str,
    run_id: str,
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> dict:
  """One historical Run's stored startup snapshot plus its pinned evidence references.

  The snapshot is the committed object of record (blocks, provenance, hash,
  char_count) — the startup injection, never a recomposition from current
  sources. Runs predating the context stage carry only a raw launch-text file:
  reported as explicitly limited legacy evidence, never fabricated into full
  provenance. Read-only and history-immutable.
  """
  run = await task_mgr.runs.get_run(session_id, run_id)
  if run is None:
    raise HTTPException(status_code=404, detail=run_not_found_in_task_text(run_id, session_id))
  from src.runtime.task_prompts import LAUNCH_TEXT_FILENAME, SNAPSHOT_FILENAME, PromptSnapshot, TaskPromptError
  snapshot_payload: dict | None = None
  legacy_prompt: dict | None = None
  if run.prompt_snapshot_ref:
    # Classification rides the recorded artifact contract, not the file's
    # current contents: the snapshot stage records ``prompt_snapshot.json``
    # (validated, provenance-carrying); a pre-stage run recorded a raw
    # launch-text path instead.
    ref_path = Path(run.prompt_snapshot_ref)
    if ref_path.name == SNAPSHOT_FILENAME:
      if not ref_path.is_file():
        raise HTTPException(status_code=500, detail=f"stored prompt snapshot missing at {run.prompt_snapshot_ref}")
      try:
        stored = json.loads(ref_path.read_text(encoding="utf-8"))
      except (OSError, ValueError) as e:
        raise HTTPException(
            status_code=500, detail=f"stored prompt snapshot unreadable at {run.prompt_snapshot_ref}: {e}") from e
      try:
        snapshot_payload = PromptSnapshot.from_json_dict(stored).to_json_dict()
      except TaskPromptError as e:
        raise HTTPException(status_code=500, detail=str(e)) from e
    else:
      # A recorded raw launch-text ref: limited historical evidence — the
      # managed instructions are visible but per-source provenance was never
      # recorded. Never recomposed into snapshot-shaped provenance.
      if not ref_path.is_file():
        raise HTTPException(status_code=500, detail=f"recorded raw launch text missing at {run.prompt_snapshot_ref}")
      legacy_prompt = _legacy_prompt_payload(ref_path)
  else:
    legacy_path = task_mgr.runs.run_dir(session_id, run_id) / LAUNCH_TEXT_FILENAME
    if legacy_path.is_file():
      legacy_prompt = _legacy_prompt_payload(legacy_path)
  return {
      "session_id": session_id,
      "run_id": run_id,
      "kind": run.kind,
      "mode": "historical",
      "snapshot": snapshot_payload,
      "legacy_prompt": legacy_prompt,
      "task_spec": ({
          "ref": run.task_spec_ref,
          "sha256": run.task_spec_hash
      } if run.task_spec_ref else None),
      "logs": {
          "raw_log_ref": run.raw_log_ref,
          "events_ref": run.events_ref,
          "result_ref": run.result_ref,
      },
  }


@router.post("/{session_id}/retry")
async def retry_session_run(
    session_id: str,
    req: RetryRunRequest,
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> dict:
  """Create the (session, request_id)-stable retry run of one recorded run.

  The retry is a queued pending execution request; dispatching the node hands
  it to the execution adapter (a stopped queued retry never launches).
  """
  if not caller.is_operator:
    raise HTTPException(status_code=403, detail="retrying a run requires operator credentials")
  try:
    result = await task_mgr.create_retry(session_id, req.request_id, req.run_id)
  except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
    raise _task_http_error(e) from e
  try:
    await task_mgr.dispatch.dispatch_pending(session_id)
  except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
    raise _task_http_error(e) from e
  return result


@router.get("/{session_id}/task-inputs/pending", response_model=PendingTaskInputsResponse)
async def list_pending_task_inputs(
    session_id: str,
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
) -> dict:
  """The task's currently pending inputs, with each one's source and text.

  The acknowledgement UI's read side: the same boundary/confirmation/claim
  folding the dispatcher and completion guards consume, never a second
  derivation. Read-only.
  """
  try:
    pending = task_mgr.dispatch.pending_inputs(session_id)
  except (TaskInvalidError, TaskNotFoundError) as e:
    raise _task_http_error(e) from e
  items = [
      {
          "id": str(event.get("id")),
          "type": str(event.get("type")),
          "timestamp": event.get("timestamp"),
          "actor": event.get("actor"),
          "source_session_id": event.get("source_session_id"),
          "from_session_name": event.get("from_session_name"),
          "text": str(event.get("content") or event.get("summary") or ""),
      } for event in pending
  ]
  return {"items": items}


@router.post("/{session_id}/task-inputs/acknowledge")
async def acknowledge_task_inputs(
    session_id: str,
    req: AcknowledgeTaskInputsRequest,
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> dict:
  """Resolve exact task inputs the operator handled out-of-band, durably and idempotently.

  Operator credentials only; every id must be a currently-pending input of this
  task (already-acknowledged ids replay as a no-op, unknown or claimed ids
  refuse with 409). The acknowledgement is a durable attributable fact naming
  the exact ids; it unblocks only those ids — later arrivals, active Runs, and
  open children keep blocking completion through the ordinary guards.
  """
  try:
    return await task_mgr.completion.acknowledge_inputs(
        session_id, request_id=req.request_id, input_ids=req.input_ids, note=req.note, caller=caller)
  except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
    raise _task_http_error(e) from e


@router.post("/{session_id}/complete")
async def complete_session_task(
    session_id: str,
    req: CompleteTaskRequest,
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> Response:
  """One completion operation: 200 Session, 202 pending_run_finish, or 409 blockers.

  A run-token agent may request closure only of its own manager task through
  its own active Run (the verified caller's bound run id — the payload never
  names one); the close re-evaluates only after that Run succeeds. Duplicate
  request ids replay the original outcome, across later epochs included.
  """
  from src.runtime import task_completion
  evidence = task_completion.CompletionEvidence(summary=req.summary, result_refs=req.result_refs, run_ids=req.run_ids)
  try:
    status, payload = await task_mgr.completion.complete_task(
        session_id, request_id=req.request_id, evidence=evidence, caller=caller)
  except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
    raise _task_http_error(e) from e
  if status == 202:
    return JSONResponse(status_code=202, content=payload)
  detail = await _completed_session_detail(task_mgr, session_id)
  return JSONResponse(status_code=200, content=detail)


async def _completed_session_detail(task_mgr: TaskTreeManager, session_id: str) -> dict:
  try:
    return await task_mgr.session_detail(session_id)
  except (TaskInvalidError, TaskNotFoundError, TaskConflictError) as e:
    raise _task_http_error(e) from e


@router.post("/{session_id}/cancel", response_model=SessionDetailResponse)
async def cancel_session_task(
    session_id: str,
    req: CancelTaskRequest,
    _meta: SessionMetadata = Depends(require_session),
    task_mgr: TaskTreeManager = Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> SessionDetailResponse:
  """Explicit cancellation with reason, preserving evidence.

  An operator cancels any open task; a run-token agent cancels only a direct
  child of its own task (403 otherwise). Active/unresolved execution or open
  children return 409; the subtree is not recursively stopped (Run cancel
  stays the separate operation).
  """
  try:
    await task_mgr.completion.cancel_task(session_id, request_id=req.request_id, reason=req.reason, caller=caller)
  except (TaskInvalidError, TaskNotFoundError, TaskForbiddenError, TaskConflictError) as e:
    raise _task_http_error(e) from e
  return SessionDetailResponse.model_validate(await _completed_session_detail(task_mgr, session_id))


def _require_own_run_scope(caller: CallerIdentity, session_id: str, run_id: str) -> None:
  """One cancel path's own-run scope: an operator passes; a run token must name this run.

  Raises 403 otherwise. The thread-cancel alias path (src/runtime/api/threads.py) calls
  this through a function-level import: sessions imports threads at module
  scope, so the reverse import is deferred to the call site.
  """
  if caller.is_operator:
    return
  claims = caller.claims
  assert claims is not None
  if claims.session_id != session_id or claims.run_id != run_id:
    raise HTTPException(status_code=403, detail="an agent may only stop its own bound run")


@router.post("/{session_id}/runs/{run_id}/cancel", response_model=RunCancelResponse)
async def cancel_session_run(
    session_id: str,
    run_id: str,
    req: CancelRunRequest,
    run_store=Depends(get_run_store),
    caller: CallerIdentity = Depends(require_caller),
) -> RunCancelResponse:
  """Request one stop: durable fact first, then identity-checked signal and exit observation."""
  _require_own_run_scope(caller, session_id, run_id)
  try:
    result = await run_store.request_stop(session_id, run_id, req.request_id)
  except (RunNotFoundError, RunIdentityConflictError) as e:
    raise _task_http_error(e) from e
  return RunCancelResponse(run_id=result.run_id, stop_requested=result.stop_requested, outcome=result.outcome)
