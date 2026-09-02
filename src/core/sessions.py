"""Session management for CharlieBot."""

import asyncio
import io
import json
import os
import shutil
import time
from collections import OrderedDict
from datetime import datetime
from pathlib import Path
from typing import Any

import aiofiles
import structlog

from src.core import event_types as ET
from src.core import plan_paths, sidebar_state
from src.core.chat_events import ChatEventStore
from src.core.config import CharlieBotConfig
from src.core.init import RUNNING_SCAN_WINDOW, iter_recent_thread_metas
from src.core.json_utils import atomic_write_text, load_json_meta, write_json_atomically
from src.core.message_aggregator import MessageAggregator
from src.core.message_projection import MessageProjection
from src.core.models import (
  TERMINAL_THREAD_STATUSES,
  BackendOption,
  CreateSessionRequest,
  MasterRunRecord,
  SessionCallbacks,
  SessionMetadata,
  SessionStatus,
  parse_utc_datetime,
  utc_now,
)
from src.core.ndjson import append_ndjson
from src.core.scheduled_sessions import (
  # re-export: src/api/cron.py imports ScheduledSessionBusyError from this module
  ScheduledSessionBusyError,
  ScheduledSessionStore,
)
from src.core.session_usage import SessionUsageResolver
from src.core.streaming import streaming_manager
from src.core.tasks import create_logged_task
from src.core.thinking_state import busy_since

# Raw event types whose render content is produced by the per-session
# MessageAggregator as `message`/`stream` deltas. We persist these events but
# do not broadcast them raw -- the deltas are the wire format.
_RAW_EVENTS_REPLACED_BY_DELTAS: frozenset[str] = frozenset({ET.ASSISTANT, ET.USER, ET.SCHEDULED_TRIGGER})

log = structlog.get_logger()

# The fork/elone API routes (src/api/sessions.py) open their auto-injected
# bootstrap prompts with these lines, and src.core.recap._AUTO_INJECTED_PREFIXES
# filters such injected messages from recap asks by prefix match; both sides
# import this one copy so an edit cannot drift the two apart.
FORK_BOOTSTRAP_OPENER = "This session continues a prior conversation."
ELONE_BOOTSTRAP_OPENER = "You're taking over because the user wasn't satisfied with the previous session."

_METADATA_CACHE_TTL = 30.0  # seconds
_SEARCH_RESULT_LIMIT = 200  # newest rows a name/content search returns; keeps the render bounded
_PROJECTION_LRU_LIMIT = 8
# LRU cap on the content-search miss memo: chat-file path -> ((mtime_ns, size, ino),
# shortest absence-proven lowercase needle). Absence of N proves every
# superstring of N absent while the file keeps that signature, so the debounced
# sidebar's growing query string re-reads a file only after the file moves.
# Chat files mutate only by append between atomic archive rewrites (inode
# swap), so a same-inode file that grew kept its old bytes: a query the stored
# needle prefixes re-proves absence by scanning the appended tail alone.
_SEARCH_MISS_MEMO_LIMIT = 256
# str.lower() and substring search hold the GIL for the whole input, so the
# sidebar content search reads chat files in windows of this many characters:
# each lower()/scan call's GIL hold stays bounded instead of scaling with the
# chat file's size, which would stall the event loop for every other request.
_SEARCH_CHUNK_CHARS = 1 << 18
# Bounds both the successor-chain walk (a cycle must never spin) and the
# delivery retry loop that re-resolves racing elones, keeping the two in agreement.
_SUCCESSOR_CHAIN_HOP_LIMIT = 100

_TRANSIENT_METADATA_FIELDS = {
    "has_running_tasks",
    "has_pending_trigger",
    "pending_trigger_count",
    "next_trigger_at",
    "has_pending_plan_approval",
    "schedule_cron",
    "schedule_enabled",
    "schedule_next_run",
    "schedule_timezone",
    "schedule_project",
    "schedule_allow_failure",
    "thinking_since",
}


def _session_channel(session_id: str) -> str:
  # Topic string must match the one session_websocket subscribes to in
  # server.py; this helper keeps this module's two publishers agreeing.
  return f"session:{session_id}"


def _stamp_thinking_since(meta: SessionMetadata) -> SessionMetadata:
  """Overwrite thinking_since with the live value before *meta* reaches a caller.

  thinking_since is a derived runtime fact owned by
  :mod:`src.core.thinking_state`; it is never persisted (see
  ``_TRANSIENT_METADATA_FIELDS``). Every API- and listing-bound return path
  (``get_session``, ``_iter_session_metas``, ``list_active_session_metas``,
  the spawn returns) applies this stamp on the way out; the succession-internal
  readers (``read_metadata_fresh``, ``resolve_successor_chain``) deliberately
  return the disk value unstamped, and a reader that needs live busy state stamps
  the meta itself (see ``_elone_scheduled_successor``). The field stays
  declared on the model, so a stale value parsed from an old metadata.json
  or restored into the cache by a post-save rebuild must not leak out
  through an unstamped API path.
  """
  meta.thinking_since = busy_since(meta.id)
  return meta


# ---------------------------------------------------------------------------
# Sidebar probe cores — pure path-in/result-out functions shared by the
# per-session probe methods below and by the poll's serial re-probe (one
# asyncio.to_thread task over all sessions instead of one task per session
# per probe group).
# ---------------------------------------------------------------------------


def has_running_tasks_sync(threads_dir: Path) -> bool:
  """True if any thread under *threads_dir* is marked 'running'.

  The 30-day-window scan (``iter_recent_thread_metas``): only threads whose
  metadata mtime is within the window are read+parsed; older thread dirs cost
  a scandir+stat with zero content reads.
  """
  for _thread_dir, _meta_path, meta in iter_recent_thread_metas(threads_dir, utc_now(), "thread_meta_read_failed"):
    if meta.get("status") == "running":
      return True
  return False


def pending_trigger_state_sync(triggers_dir: Path) -> tuple[int, datetime | None]:
  """(pending trigger count, earliest fire time) from the *.json files under *triggers_dir*."""
  if not triggers_dir.exists():
    return 0, None

  pending_count = 0
  next_trigger_at: datetime | None = None
  for trigger_path in triggers_dir.glob("*.json"):
    trigger = load_json_meta(
        trigger_path,
        "trigger_meta_read_failed",
        catch=(OSError, ValueError),
    )
    if trigger is None:
      continue
    if trigger.get("status") != "pending":
      continue

    pending_count += 1
    fire_at = SessionManager._parse_optional_utc(
        trigger.get("fire_at"), "trigger_fire_at_parse_failed", trigger_path=str(trigger_path))
    if fire_at is None:
      continue
    if next_trigger_at is None or fire_at < next_trigger_at:
      next_trigger_at = fire_at

  return pending_count, next_trigger_at


def has_pending_plan_approval_sync(plans_path: Path, session_id: str) -> bool:
  """True if any lineage in the plans.json at *plans_path* is 'awaiting approval'.

  Delegates to the tolerant read in src.core.plans (single authority for
  catch-and-derive). Any error entry is logged via ``plan_registry_read_failed``
  and contributes no pending approval. The probe must never raise — a corrupt
  single-session file cannot 5xx the sidebar poll for all sessions.
  """
  # lazy: plans imports SessionManager from this module at top level
  from src.core.plans import read_plans_tolerant

  result = read_plans_tolerant(plans_path, session_id)
  for error in result["errors"]:
    log.warning(
        "plan_registry_read_failed",
        session_id=error.get("session_id"),
        error=error.get("error"),
    )
  return any(plan.get("state") == "awaiting approval" for plan in result["plans"])


def _scan_content_for_hit(path: Path, session_id: str, query_lower: str, start: int) -> bool | None:
  """Character-window scan of a chat-events file for *query_lower* (thread-pool work).

  *start* is a byte offset the scan begins at; the caller uses it to re-scan
  only the bytes appended after a proven-absent prefix. The offset can split
  a UTF-8 sequence, so a nonzero start decodes with ``errors="replace"`` and
  the verdict equals a full strict scan's for every file a full scan can
  decode. A zero start keeps strict decoding: a corrupt file fails loud as
  before. Returns the verdict, or None when the file could not be read: an
  errored scan proves no absence, so the caller must not memoize it as a miss.
  """
  overlap = len(query_lower) - 1
  tail = ""
  try:
    with path.open("rb") as raw:
      if start:
        raw.seek(start)
      with io.TextIOWrapper(raw, encoding="utf-8", errors="replace" if start else "strict") as stream:
        while True:
          chunk = stream.read(_SEARCH_CHUNK_CHARS)
          if not chunk:
            return False
          window = tail + chunk.lower()
          if query_lower in window:
            return True
          # This and the next window together cover the file with an
          # overlap of len(query)-1 chars, so a hit straddling the chunk
          # boundary lies whole inside exactly one window.
          tail = window[-overlap:] if overlap else ""
  except OSError as e:
    log.debug("search_read_failed", session_id=session_id, error=str(e))
    return None


def probe_sidebar_state_sync(
    specs: list[tuple[str, Path, Path, Path]],
) -> dict[str, dict]:
  """Probe every ``(session_id, threads_dir, triggers_dir, plans_path)`` spec serially.

  The deep-probe core of a sidebar re-probe: all three probe groups per
  session, one session at a time, so probing N sessions costs one thread-pool
  task instead of 3*N. Returns
  ``{session_id: {"thread_running", "pending_trigger_count", "next_trigger_at",
  "has_pending_plan_approval"}}``.
  """
  results: dict[str, dict] = {}
  for session_id, threads_dir, triggers_dir, plans_path in specs:
    running = has_running_tasks_sync(threads_dir)
    pending_count, next_trigger_at = pending_trigger_state_sync(triggers_dir)
    results[session_id] = {
        "thread_running": running,
        "pending_trigger_count": pending_count,
        "next_trigger_at": next_trigger_at,
        "has_pending_plan_approval": has_pending_plan_approval_sync(plans_path, session_id),
    }
  return results


def _sidebar_probe_signature(threads_dir: Path, triggers_dir: Path, plans_path: Path) -> tuple:
  """Stat-only identity of every byte the sidebar probe reads, for one session.

  A deep probe's result can change only three ways, and the signature pins all
  three: a probed file's content changes (caught by ``(st_mtime_ns, st_size)``
  for both atomic-rename and in-place writers), a probed file or thread dir
  appears or disappears (caught by the sorted name sets, with ``None`` marking
  a thread dir still missing its metadata.json), or the 30-day
  ``RUNNING_SCAN_WINDOW`` rolls past a metadata's mtime and drops it from
  ``has_running_tasks_sync``'s read set without any file changing (caught by
  the rollover element: the earliest ``mtime + window`` over scanned metas, or
  ``float('inf')`` when nothing was scanned). The stat pass mirrors the probe
  cores' own scandir+stat phase, so a signature sweep costs the cheap half of
  a probe and skips every content read and parse.
  """
  thread_sig = []
  rollovers = []
  if threads_dir.is_dir():
    with os.scandir(threads_dir) as entries:
      for entry in entries:
        if not entry.is_dir():
          continue
        try:
          st = (Path(entry.path) / "metadata.json").stat()
        except OSError:
          thread_sig.append((entry.name, None))
          continue
        thread_sig.append((entry.name, st.st_mtime_ns, st.st_size))
        rollovers.append(st.st_mtime + RUNNING_SCAN_WINDOW.total_seconds())
  trigger_sig = []
  if triggers_dir.is_dir():
    with os.scandir(triggers_dir) as entries:
      for entry in entries:
        if not entry.name.endswith(".json"):
          continue
        try:
          st = entry.stat()
        except OSError:
          continue
        trigger_sig.append((entry.name, st.st_mtime_ns, st.st_size))
  try:
    plans_st = plans_path.stat()
    plans_sig: tuple | None = (plans_st.st_mtime_ns, plans_st.st_size)
  except OSError:
    plans_sig = None
  rollover = min(rollovers) if rollovers else float("inf")
  return (tuple(sorted(thread_sig)), tuple(sorted(trigger_sig)), plans_sig, rollover)


def _sidebar_signature_fresh(session_id: str, signature: tuple, now_ts: float) -> bool:
  """True when the stored signature equals *signature* and its scan-window rollover is still ahead."""
  stored = sidebar_state.probe_signature(session_id)
  return stored == signature and now_ts < signature[3]


def selective_probe_sidebar_state(
    specs: list[tuple[str, Path, Path, Path]],
    *,
    deep: bool,
) -> tuple[dict[str, dict], dict[str, tuple]]:
  """Deep-probe exactly the specs whose probe inputs changed since the last probe.

  Composes one signature stat pass with :func:`probe_sidebar_state_sync`-shaped
  probing of the changed specs so one thread-pool task covers selection and
  execution. ``deep=True`` probes every spec regardless of signatures (the
  ``/status?force=1`` escape hatch). Returns ``(entries, signatures)``; the
  caller stores both in :mod:`src.core.sidebar_state` on the event loop.
  """
  sigs: dict[str, tuple] = {}
  to_probe: list[tuple[str, Path, Path, Path]] = []
  now_ts = time.time()
  for session_id, threads_dir, triggers_dir, plans_path in specs:
    sig = _sidebar_probe_signature(threads_dir, triggers_dir, plans_path)
    sigs[session_id] = sig
    if deep or not _sidebar_signature_fresh(session_id, sig, now_ts):
      to_probe.append((session_id, threads_dir, triggers_dir, plans_path))
  return probe_sidebar_state_sync(to_probe), sigs


class SuccessionRefused(ValueError):
  """An elone was refused because a scheduler-owned parent cannot take a new successor.

  Only a scheduler-owned parent (``scheduled_task`` set) that already has a
  successor refuses; ordinary parents re-elone freely. Carries the
  successor-already-set rejection so the API can answer 409 while other
  ValueErrors keep answering 400.
  """


class SessionManager:
  """CRUD operations for CharlieBot sessions."""

  def __init__(self, cfg: CharlieBotConfig) -> None:
    self._cfg = cfg
    # In-memory metadata cache: session_id -> (metadata, monotonic_timestamp).
    # TTL-based to avoid repeated disk reads within the same poll cycle.
    self._metadata_cache: dict[str, tuple[SessionMetadata, float]] = {}
    # Per-session asyncio.Lock guarding metadata read-modify-write operations.
    # Prevents clobber races between concurrent mutators (e.g. mark_unread vs
    # update_thinking_state), which both load meta, mutate disjoint fields, and
    # save back — without a lock the second save overwrites the first's change.
    self._metadata_locks: dict[str, asyncio.Lock] = {}
    self._chat_events = ChatEventStore(self._session_dir, self._metadata_path, self._metadata_cache)
    self._session_usage = SessionUsageResolver(
        cfg,
        self._chat_events.events_cache,
        self.get_chat_events_path,
        self.load_chat_events_sync,
    )
    self._scheduled_sessions = ScheduledSessionStore(self)
    # Per-session MessageAggregator instance carrying live streaming state
    # (assistant_buf, tools_buf). Lazy-initialized from disk on first
    # persist_and_broadcast for a session after server start, then maintained
    # in memory across calls so consecutive assistant chunks accumulate into
    # a single bubble and tool-only events attach to the prior text bubble.
    self._aggregators: dict[str, MessageAggregator] = {}
    # Per-session MessageProjection cache (LRU, cap _PROJECTION_LRU_LIMIT). A
    # hit requires the cached event_count to equal the live event count
    # (get_message_projection), so a stale projection is never served.
    self._projection_cache: OrderedDict[str, MessageProjection] = OrderedDict()
    self._search_miss_memo: OrderedDict[str, tuple[tuple[int, int, int], str]] = OrderedDict()

  # ---------------------------------------------------------------------------
  # Session CRUD
  # ---------------------------------------------------------------------------

  async def create_session(self, req: CreateSessionRequest, backend: str | None = None) -> SessionMetadata:
    """Create a new session."""
    name = req.name or await self._next_session_name()
    overrides: dict[str, str] = {}
    if req.session_id:
      overrides["id"] = req.session_id
    meta = SessionMetadata(
        name=name,
        scheduled_task=req.scheduled_task,
        role=req.role,
        backend=backend or self._cfg.backend_options[0].id,
        slack_origin=req.slack_origin,
        **overrides)

    self._create_session_dirs(self._session_dir(meta.id))

    await self.save_metadata(meta)
    await self._backend_create_hook(meta)

    log.info("session_created", session_id=meta.id, name=meta.name)
    return _stamp_thinking_since(meta)

  async def ensure_scheduled_session_backend(
      self,
      task_name: str,
      backend: str,
      session_cache: dict[str, list[SessionMetadata]] | None = None,
      skip_if_busy: bool = False,
      role: str | None = None,
      group: str | None = None,
  ) -> SessionMetadata | None:
    """Return the active scheduled session for task_name/backend, rotating history if needed.

    Backend changes are generation changes: the old active session is archived and a new
    scheduled session is created with only scheduler bookkeeping copied over. ``role`` and
    ``group`` (mode: master PM tasks) carry onto the created session.
    """
    return await self._scheduled_sessions.ensure_scheduled_session_backend(
        task_name,
        backend,
        session_cache,
        skip_if_busy,
        role,
        group,
    )

  def _tui_cli_option(self, backend_id: str) -> BackendOption | None:
    # Only tui-cli backends carry tmux lifecycle state, and the create and
    # destroy hooks must agree on which sessions that covers.
    option = self._cfg.get_backend_option(backend_id)
    return option if option is not None and option.type == "tui-cli" else None

  async def _backend_create_hook(self, meta: SessionMetadata) -> None:
    """Run backend-specific session-create work (e.g. spawn tmux for tui-cli)."""
    if self._tui_cli_option(meta.backend) is None:
      return
    from src.agents.backends.tui import ensure_tmux_session
    try:
      await ensure_tmux_session(meta.id, self._session_dir(meta.id))
    except Exception:
      log.exception("backend_create_hook_failed", session_id=meta.id, backend=meta.backend)
      raise

  async def _backend_destroy_hook(self, session_id: str, meta: SessionMetadata | None) -> None:
    """Run backend-specific teardown (e.g. kill tmux for tui-cli)."""
    # Only called on permanent delete. Archive is a status-only flag and must
    # NOT kill the underlying tmux/claude process for tui-cli sessions.
    if meta is None:
      return
    if self._tui_cli_option(meta.backend) is None:
      return
    from src.agents.backends.tui import kill_tmux_session
    await kill_tmux_session(session_id)

  async def get_session(self, session_id: str) -> SessionMetadata | None:
    """Load session metadata, using in-memory cache when available."""
    meta = self._fresh_cached_meta(session_id)
    cache_hit = meta is not None
    if meta is None:
      raw = await self._read_metadata_raw(session_id)
      if raw is None:
        return None
      meta = SessionMetadata.model_validate_json(raw)
    # The migrate branch's save_metadata re-populates the cache, so the manual
    # populate below covers disk loads only; re-stamping a hit's timestamp
    # would wrongly extend its TTL.
    if self._migrate_round_rating_keys(meta):
      await self.save_metadata(meta)
    elif not cache_hit:
      self._metadata_cache[session_id] = (meta, time.monotonic())
    return _stamp_thinking_since(meta.model_copy())

  async def _get_session_bypassing_cache(self, session_id: str) -> SessionMetadata | None:
    """get_session forced past the TTL cache, so the read lands on disk.

    The single-field mutators (``persist_cc_session_id``, ``persist_master_run``,
    ``update_thinking_state``) and ``persist_cc_session_id``'s post-save
    read-back must act on the latest on-disk state, not a TTL-cached view: a
    stale view would clobber a concurrent writer's save. Unlike
    ``read_metadata_fresh`` this stays a ``get_session`` call — the rating-key
    migration still runs and the cache is re-populated from the read. Hold
    ``self._lock_for(session_id)`` around the whole mutate-save; without the
    lock the fresh view races other writers.
    """
    self._invalidate_cache(session_id)
    return await self.get_session(session_id)

  async def read_metadata_fresh(self, session_id: str) -> SessionMetadata | None:
    """Read metadata.json directly from disk, bypassing ``_metadata_cache``.

    The cache is TTL-based and can be stale relative to a concurrent elone, so
    succession resolution always reads fresh. Returns None when the file is
    absent or blank. Does not populate the cache from the read.
    """
    raw = await self._read_metadata_raw(session_id)
    if raw is None:
      return None
    return SessionMetadata.model_validate_json(raw)

  async def _read_metadata_raw(self, session_id: str) -> str | None:
    """Return the raw metadata.json text, or None when the file is missing or blank.

    Single read tail for the one-session-at-a-time metadata readers (cached
    ``get_session`` and bypassing ``read_metadata_fresh``): a blank file must
    warn exactly once per read through the same ``session_metadata_empty``
    event. The listing path (``_iter_session_metas``) cannot route through
    here — it batches all missing metadata reads synchronously in one
    ``asyncio.to_thread`` call — and keeps its own absent/blank handling with
    the same warning event.
    """
    path = self._metadata_path(session_id)
    if not path.exists():
      return None
    async with aiofiles.open(path) as f:
      raw = await f.read()
    if not raw.strip():
      log.warning("session_metadata_empty", session_id=session_id, path=str(path))
      return None
    return raw

  async def resolve_successor_chain(self, session_id: str) -> SessionMetadata | None:
    """Walk ``successor_session_id`` from *session_id* to the chain end.

    Uses ``read_metadata_fresh`` at every hop (never the TTL cache, which may
    be stale relative to a concurrent elone). Returns the chain-end metadata, or
    None when *session_id* itself has no metadata on disk. When a hop's
    successor id has no metadata (that session was permanently deleted), stops
    and returns the last session that does exist, logging one structured error.
    Raises RuntimeError if the chain exceeds ``_SUCCESSOR_CHAIN_HOP_LIMIT``
    hops (a cycle must never spin).
    """
    current = await self.read_metadata_fresh(session_id)
    if current is None:
      return None
    hops = 0
    while current.successor_session_id is not None:
      if hops >= _SUCCESSOR_CHAIN_HOP_LIMIT:
        raise RuntimeError(
            f"successor chain from {session_id} exceeds {_SUCCESSOR_CHAIN_HOP_LIMIT} hops;"
            " aborting to avoid a cycle")
      successor_id = current.successor_session_id
      successor = await self.read_metadata_fresh(successor_id)
      if successor is None:
        log.error(
            "successor_chain_broken",
            session_id=session_id,
            missing_session_id=successor_id,
        )
        return current
      current = successor
      hops += 1
    return current

  async def deliver_to_successor(self, session_id: str, event: dict) -> str | None:
    """Persist *event* into the session currently ending *session_id*'s succession chain.

    Resolves the chain end via ``resolve_successor_chain``, then holds that tail's
    lock while re-resolving (an elone may have landed while we waited), confirming the
    tail's directory still exists, and persisting. Returns the id of the session
    actually written to, or None when nothing could be written (chain end's metadata
    was permanently deleted). Redirected events (chain end differs from *session_id*)
    are stamped with ``event['origin_session_id'] = session_id``.
    """
    # One opening attempt plus up to _SUCCESSOR_CHAIN_HOP_LIMIT tail-chases;
    # fail loud rather than looping forever on a steady stream of landing elones.
    for _ in range(_SUCCESSOR_CHAIN_HOP_LIMIT + 1):
      tail = await self.resolve_successor_chain(session_id)
      if tail is None:
        log.error("deliver_to_successor_origin_missing", session_id=session_id)
        return None

      async with self._lock_for(tail.id):
        # Re-resolve from the tail with a fresh read: an elone may have landed
        # while we waited for the lock. If so, release and repeat from the top.
        fresh_tail = await self.read_metadata_fresh(tail.id)
        if fresh_tail is None:
          log.error("deliver_to_successor_tail_missing", session_id=session_id, tail_id=tail.id)
          return None
        if fresh_tail.successor_session_id is not None:
          continue
        # The tail's metadata.json exists (confirmed above), so append_ndjson will
        # never recreate a directory for a permanently deleted session.
        if not self._metadata_path(fresh_tail.id).exists():
          log.error(
              "deliver_to_successor_metadata_missing",
              session_id=session_id,
              tail_id=fresh_tail.id,
          )
          return None

        if fresh_tail.id != session_id:
          event["origin_session_id"] = session_id
        await self.persist_and_broadcast(fresh_tail.id, event)
        return fresh_tail.id

    raise RuntimeError(
        f"deliver_to_successor from {session_id} retried {_SUCCESSOR_CHAIN_HOP_LIMIT}"
        " times; aborting to avoid a loop")

  async def list_sessions(
      self,
      status: SessionStatus | None = None,
      starred: bool | None = None,
      scheduled: bool | None = None,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
  ) -> list[SessionMetadata]:
    """List sessions, newest first. Optionally filter by status, starred, and/or scheduled."""
    all_meta = await self._iter_session_metas(status=status)
    sessions = [
        meta for meta in all_meta
        if (starred is None or meta.starred == starred) and (scheduled is None or bool(meta.scheduled_task) == scheduled)
    ]
    return await self._enrich_and_sort(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
        include_pending_plan_approval=include_pending_plan_approval,
    )

  async def list_archived_page(
      self,
      *,
      group: str | None = None,
      limit: int = 100,
      before: str | None = None,
      before_id: str | None = None,
  ) -> dict:
    """One page of archived sessions, newest first, plus group aggregates.

    ``group`` picks membership: None = every archived session, "" = the
    ungrouped ones, a name = that group. ``limit`` clamps to 1..500. The
    keyset cursor ``(before, before_id)`` names the previous page's last row;
    rows strictly after it in ``(updated_at, id)``-descending order form the
    next page, so a row archived or deleted between fetches never shifts the
    page boundary. ``groups`` aggregates the whole archived set (not the
    current filter) for the filter strip: named groups alphabetically, the
    ungrouped bucket (group=None) last. Ordering and aggregation are computed
    per request from the cache — archived entries never expire there
    (``_fresh_cached_meta``), so the warm request path reads no metadata
    files. A cursor that fails to parse raises ValueError: the caller's
    explicit cursor stops with the error instead of silently serving page 1.
    """
    limit = max(1, min(500, limit))
    metas = await self._load_session_metas(status=SessionStatus.ARCHIVED)

    counts: dict[str | None, int] = {}
    for meta in metas:
      key = meta.group or None
      counts[key] = counts.get(key, 0) + 1
    groups = [{"group": name, "total": counts[name]} for name in sorted(k for k in counts if k is not None)]
    if None in counts:
      groups.append({"group": None, "total": counts[None]})

    if group is None:
      rows = list(metas)
    elif group == "":
      rows = [meta for meta in metas if not meta.group]
    else:
      rows = [meta for meta in metas if meta.group == group]
    rows.sort(key=lambda meta: (meta.updated_at, meta.id), reverse=True)

    if (before is None) != (before_id is None):
      raise ValueError("before and before_id form one cursor: pass both or neither")
    if before is not None:
      cursor_time = datetime.fromisoformat(before)
      if cursor_time.tzinfo is None:
        raise ValueError("before must be a timezone-aware ISO timestamp")
      cursor = (cursor_time, before_id)
      rows = [meta for meta in rows if (meta.updated_at, meta.id) < cursor]

    page = rows[:limit]
    has_more = len(rows) > limit
    sessions = [_stamp_thinking_since(meta.model_copy()) for meta in page]
    await self.populate_sidebar_state(
        sessions,
        include_running_status=True,
        include_pending_trigger_status=True,
    )
    return {
        "sessions": sessions,
        "has_more": has_more,
        "next_before": page[-1].updated_at.isoformat() if page and has_more else None,
        "next_before_id": page[-1].id if page and has_more else None,
        "groups": groups,
    }

  async def search_sessions(
      self,
      query: str,
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
  ) -> list[SessionMetadata]:
    """Search sessions by name (every status) and chat event content (active only), case-insensitive.

    Returns at most ``_SEARCH_RESULT_LIMIT`` rows, newest first: the cap keeps
    the render bounded when a short query matches thousands of archived names.
    """
    query_lower = query.lower()
    all_meta = await self._load_session_metas()
    results: list[SessionMetadata] = []
    content_candidates: list[tuple[SessionMetadata, Path]] = []
    for meta in all_meta:
      if query_lower in meta.name.lower():
        results.append(_stamp_thinking_since(meta.model_copy()))
        continue
      if meta.status == SessionStatus.ACTIVE:
        content_candidates.append((meta, self.get_chat_events_path(meta.id)))

    async def _check_content(meta: SessionMetadata, path: Path) -> SessionMetadata | None:
      """Check if a session's chat events contain the query (runs file I/O in thread pool)."""
      memo_key = str(path)
      memo_entry = self._search_miss_memo.get(memo_key)

      def _read_and_check() -> tuple[bool, tuple[int, int, int] | None]:
        try:
          stat = path.stat()
        except OSError as e:
          log.debug("search_read_failed", session_id=meta.id, error=str(e))
          return False, None
        sig = (stat.st_mtime_ns, stat.st_size, stat.st_ino)
        start = 0
        if memo_entry is not None and query_lower.startswith(memo_entry[1]):
          if memo_entry[0] == sig:
            return False, None  # proven-absent memo verdict: no file read
          old_size, old_ino = memo_entry[0][1], memo_entry[0][2]
          if old_ino == stat.st_ino and stat.st_size > old_size:
            # Same inode and larger: appends only, so the stored needle's
            # absence still holds over the old bytes. A query occurrence
            # crossing the old size starts at most 4 bytes per char earlier,
            # hence the seek window; +8 covers decode resync at the offset.
            start = max(0, old_size - (4 * len(query_lower) + 8))
        verdict = _scan_content_for_hit(path, meta.id, query_lower, start)
        if verdict is None:
          return False, None
        return verdict, sig

      hit, sig = await asyncio.to_thread(_read_and_check)
      if hit:
        return _stamp_thinking_since(meta.model_copy())
      if sig is not None:
        self._memoize_search_miss(memo_key, sig, query_lower)
      return None

    content_hits = await asyncio.gather(*(_check_content(m, p) for m, p in content_candidates))
    results.extend(meta for meta in content_hits if meta is not None)
    enriched = await self._enrich_and_sort(
        results,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
    )
    return enriched[:_SEARCH_RESULT_LIMIT]

  def _memoize_search_miss(self, memo_key: str, sig: tuple[int, int, int], needle: str) -> None:
    """Record a clean-scan miss under the shorter of the known needles.

    The shorter needle subsumes more superstrings, so an existing entry under
    the same signature wins when it is no longer than the fresh one; a moved
    signature (append) always replaces with the new tail scan's signature,
    since the old entry says nothing about the appended bytes.
    """
    existing = self._search_miss_memo.get(memo_key)
    if existing is not None and existing[0] == sig and len(existing[1]) <= len(needle):
      return
    self._search_miss_memo[memo_key] = (sig, needle)
    self._search_miss_memo.move_to_end(memo_key)
    while len(self._search_miss_memo) > _SEARCH_MISS_MEMO_LIMIT:
      self._search_miss_memo.popitem(last=False)

  async def fork_session(
      self,
      parent_id: str,
      event_index: int | None = None,
      backend: str | None = None,
  ) -> SessionMetadata:
    """Create a new session with parent events stored as a reference file."""
    meta = await self._spawn_with_reference(parent_id, event_index, backend, "C")
    self._log_spawn("session_cloned", meta, parent_id, event_index)
    return meta

  async def elone_session(
      self,
      parent_id: str,
      event_index: int,
      backend: str | None = None,
  ) -> SessionMetadata:
    """Create an Elon-e session: reference handoff, archive + thumbs-down parent.

    Runs the succession rejection BEFORE any child session is created, so a
    refused call mutates nothing on disk. The per-parent invariant: each elone
    overwrites ``successor_session_id`` so the pointer names the parent's most
    recent elone child. Ordinary parents re-elone freely; scheduler-owned
    parents refuse once they already have a successor. Consumers (chain
    resolution, delivery, trigger redirect) follow the pointer and therefore
    land at the most recent takeover. A scheduler-owned parent takes an
    inheriting succession: the child carries the scheduling identity and
    bookkeeping, and the task yaml's backend is written back before the parent
    is archived, so the alignment scan never has rotation work to do.
    """
    fresh_parent = await self.read_metadata_fresh(parent_id)
    if fresh_parent is None:
      raise FileNotFoundError(f"parent session not found: {parent_id}")
    if fresh_parent.successor_session_id is not None and fresh_parent.scheduled_task is not None:
      raise SuccessionRefused(
          f"session {parent_id} already has a successor "
          f"({fresh_parent.successor_session_id}); elone that successor or fork for a separate branch")
    if fresh_parent.scheduled_task is not None:
      meta = await self._elone_scheduled_successor(fresh_parent, event_index, backend)
    else:
      meta = await self._spawn_with_reference(parent_id, event_index, backend, "E")

    # Auto-archive and thumbs-down the parent, and record the elone successor
    # pointer (re-read under lock so concurrent mutations to the parent aren't
    # clobbered). Latest-wins: an ordinary parent's pointer is overwritten to
    # name each new child, so the pointer always names the most recent elone.
    # Scheduler-owned parents refuse re-elone, so their pointer stays write-once.
    async with self._lock_for(parent_id):
      fresh_parent = await self.get_session(parent_id)
      if fresh_parent:
        fresh_parent.status = SessionStatus.ARCHIVED
        fresh_parent.rating = "thumbs_down"
        fresh_parent.successor_session_id = meta.id
        fresh_parent.updated_at = utc_now()
        await self.save_metadata(fresh_parent)
    self._drop_session_runtime_state(parent_id)

    self._log_spawn("session_eloned", meta, parent_id, event_index)
    return meta

  async def _elone_scheduled_successor(
      self,
      parent: SessionMetadata,
      event_index: int,
      backend: str | None,
  ) -> SessionMetadata:
    """Spawn the inheriting successor for a scheduler-owned elone parent.

    Step order keeps one active session matching the task yaml at every
    instant, so a scheduler alignment scan landing mid-succession has no
    rotation work to do: busy check -> inheriting spawn -> backend write-back;
    the parent is archived afterwards by the shared elone flow. A busy parent
    refuses with the same exception type the rotation path raises. A failed
    write-back archives the successor just created, leaves the parent
    untouched, and re-raises the original error, returning to pre-succession
    state.
    """
    # read_metadata_fresh carries no thinking_since (a derived runtime fact);
    # stamp it so the shared busy predicate sees the live busy state, exactly
    # as the stamped metas the rotation path consults do.
    if await self._scheduled_sessions._scheduled_session_busy(_stamp_thinking_since(parent)):
      raise ScheduledSessionBusyError(
          f"scheduled task '{parent.scheduled_task}' elone is blocked because session "
          f"'{parent.id}' has running work; retry when it is idle")
    meta = await self._spawn_with_reference(parent.id, event_index, backend, "E", inherit_scheduling=True)
    try:
      await self._scheduled_sessions.write_scheduled_task_backend(parent.scheduled_task, meta.backend)
    except Exception:
      await self.archive_session(meta.id)
      raise
    return meta

  async def _spawn_with_reference(
      self,
      parent_id: str,
      event_index: int | None,
      backend: str | None,
      name_prefix: str,
      inherit_scheduling: bool = False,
  ) -> SessionMetadata:
    """Create a child session whose parent history lives in data/parent_reference.jsonl.

    ``inherit_scheduling`` marks an inheriting scheduler succession: the child
    keeps the parent name verbatim (name_prefix goes unused), takes over
    scheduled_task and role, and receives the scheduler bookkeeping so the
    next cron tick sees an unbroken cadence.
    """
    parent = await self.get_session(parent_id)
    if not parent:
      raise FileNotFoundError(f"parent session not found: {parent_id}")

    count = await asyncio.to_thread(self.get_chat_event_count_sync, parent_id, parent)
    if event_index is None:
      end = count
    else:
      if event_index < 0 or event_index >= count:
        raise ValueError(f"event_index {event_index} out of range for parent session {parent_id} with {count} events")
      end = event_index + 1

    # A full-corpus reference carries every parent event, so raw lines move it
    # verbatim; the parse-and-reserialize round trip only buys the truncation a
    # takeover point asks for.
    if event_index is None:
      reference_raw = await asyncio.to_thread(self._read_reference_raw_sync, parent_id, end)
    else:
      events, _ = await asyncio.to_thread(self.load_chat_events_range, parent_id, 0, end)
      if len(events) != end:
        raise ValueError(f"loaded {len(events)} parent events for requested range [0, {end})")
      reference_raw = await asyncio.to_thread(self._serialize_reference_events_sync, events)

    meta = SessionMetadata(
        name=parent.name if inherit_scheduling else f"{name_prefix}{parent.name}",
        parent_session_id=parent_id,
        backend=backend or parent.backend,
        group=parent.group,
    )
    if inherit_scheduling:
      meta.scheduled_task = parent.scheduled_task
      meta.role = parent.role
      self._scheduled_sessions.migrate_scheduler_bookkeeping(parent, meta)
    session_dir = self._session_dir(meta.id)
    self._create_session_dirs(session_dir)

    reference_path = self.parent_reference_path(meta.id)
    await asyncio.to_thread(self._write_reference_raw_sync, reference_path, reference_raw)

    events_path = self.get_chat_events_path(meta.id)
    clone_event = {
        "type": ET.CLONE_START,
        "parent_session_id": parent_id,
        "parent_session_name": parent.name,
        "timestamp": utc_now().isoformat(),
    }
    await append_ndjson(events_path, clone_event)
    await self.save_metadata(meta)
    await self._copy_plans_to_child(parent_id, session_dir)
    # The child plans.json is written by _copy_plans_sync (not PlanRegistryManager._save),
    # and a poll racing between save_metadata and this copy could have snapshotted
    # the child without plans; re-mark so the copied registry is always probed.
    sidebar_state.mark_sidebar_dirty(meta.id)
    return _stamp_thinking_since(meta)

  async def _copy_plans_to_child(self, parent_id: str, child_session_dir: Path) -> None:
    """Copy parent plans.json and every referenced artifact file into the child.

    The child registry rewrites each ``versions[].file`` to a POSIX path
    relative to the child session directory. All other registry fields carry
    over unchanged. A missing or outside-parent artifact logs a warning and
    does not abort the fork. No plans.json in the parent means nothing to copy.
    """
    parent_dir = self._session_dir(parent_id).resolve()
    parent_plans_path = parent_dir / "plans.json"
    if not parent_plans_path.exists():
      return
    await asyncio.to_thread(self._copy_plans_sync, parent_plans_path, parent_dir, child_session_dir.resolve())

  @staticmethod
  def _copy_plans_sync(parent_plans_path: Path, parent_dir: Path, child_dir: Path) -> None:
    raw = parent_plans_path.read_text(encoding="utf-8")
    data = json.loads(raw)
    # Every in-parent relative path must be reserved before any outside-parent
    # fallback is chosen, so a fallback can never alias an artifact a later
    # version copies.
    resolved: list[tuple[dict, dict, Path, Path | None]] = []
    reserved_relative_paths = {"plans.json"}
    for plan in data.get("plans", []):
      for ver in plan.get("versions", []):
        file_rel = ver.get("file")
        if not file_rel:
          continue
        candidate, normalized_rel = plan_paths.resolve_plan_file(parent_dir, file_rel)
        resolved.append((plan, ver, candidate, normalized_rel))
        if normalized_rel is not None:
          reserved_relative_paths.add(normalized_rel.as_posix())

    for plan, ver, candidate, resolved_rel in resolved:
      normalized_rel = resolved_rel
      inside_parent = normalized_rel is not None
      if normalized_rel is None:
        fallback_rel = plan_paths.fallback_relative_path(parent_dir, candidate)
        normalized_rel = fallback_rel
        suffix_number = 1
        while (normalized_rel.as_posix() in reserved_relative_paths or (child_dir / normalized_rel).exists()):
          normalized_rel = fallback_rel.with_name(f"{fallback_rel.name}.outside-{suffix_number}")
          suffix_number += 1
        reserved_relative_paths.add(normalized_rel.as_posix())
        log.warning(
            "plan_artifact_outside_parent_on_fork",
            file=str(candidate),
            relative_file=normalized_rel.as_posix(),
            plan=plan.get("id"),
            v=ver.get("v"),
        )
      src = parent_dir / normalized_rel
      dst = child_dir / normalized_rel
      ver["file"] = normalized_rel.as_posix()
      if not inside_parent:
        continue
      if not src.exists():
        log.warning("plan_artifact_missing_on_fork", file=str(src), plan=plan.get("id"), v=ver.get("v"))
        continue
      dst.parent.mkdir(parents=True, exist_ok=True)
      shutil.copy2(src, dst)
    child_plans_path = child_dir / "plans.json"
    child_plans_path.parent.mkdir(parents=True, exist_ok=True)
    write_json_atomically(child_plans_path, data, indent=2)

  @staticmethod
  def _serialize_reference_events_sync(events: list[dict]) -> str:
    return "".join(json.dumps(event) + "\n" for event in events)

  def _read_reference_raw_sync(self, parent_id: str, end: int) -> str:
    """Concatenate the parent's raw event lines for a full-corpus reference.

    Returns the non-blank lines among the first ``archive_take`` raw archive
    lines (``data/archives/`` in the chronological filename glob) followed by
    the non-blank lines among the first ``end - archive_take`` raw live lines,
    with ``archive_take`` the read-time archive offset capped at ``end``. This
    is the event sequence ``load_chat_events_range`` parses for the range
    ``[0, end)``: both sides read the offset when the read starts — an archive
    pass landing between the caller's event count and this read moves lines
    from the live file to the archive tail, changing the split but not the
    sequence — and both spend raw lines against the budget and skip blanks.
    The corrupt-corpus failures the parsed write surfaces through its length
    check stay loud here without paying the parse: a non-blank line that a
    serialized event dict cannot produce (not wrapped in ``{}``) raises, and so
    does a take that ends short.
    """
    archive_take = min(self._chat_events.read_archive_offset_sync(parent_id), end)
    live_take = end - archive_take
    parent_dir = self._session_dir(parent_id)
    parts: list[str] = []

    def take_lines(path: Path, take: int) -> tuple[int, int]:
      raw = 0
      appended = 0
      with open(path, encoding="utf-8") as stream:
        for line in stream:
          if raw >= take:
            break
          raw += 1
          stripped = line.strip()
          if not stripped:
            continue
          if not stripped.startswith("{") or not stripped.endswith("}"):
            raise ValueError(f"parent event line is not a serialized event object: {stripped[:80]!r}")
          parts.append(line if line.endswith("\n") else line + "\n")
          appended += 1
      return raw, appended

    archives_dir = parent_dir / "data" / "archives"
    raw_left = archive_take
    archived = 0
    if raw_left and archives_dir.is_dir():
      for path in sorted(archives_dir.glob("chat_events.*.jsonl")):
        if raw_left <= 0:
          break
        raw, appended = take_lines(path, raw_left)
        raw_left -= raw
        archived += appended
    if archived != archive_take:
      raise ValueError(f"loaded {archived} archived parent events for requested range [0, {archive_take})")

    live_path = self.get_chat_events_path(parent_id)
    live = 0
    if live_take and live_path.exists():
      _, live = take_lines(live_path, live_take)
    if live != live_take:
      raise ValueError(f"loaded {live} live parent events for requested range [0, {live_take})")
    return "".join(parts)

  @staticmethod
  def _write_reference_raw_sync(path: Path, raw: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    atomic_write_text(path, raw)

  @staticmethod
  def _create_session_dirs(session_dir: Path) -> None:
    # Every session-creation path (create_session, _spawn_with_reference) lays
    # down the identical skeleton; the single helper is what keeps them agreeing.
    for subdir in ("data", "threads"):
      (session_dir / subdir).mkdir(parents=True, exist_ok=True)

  @staticmethod
  def _log_spawn(event: str, meta: SessionMetadata, parent_id: str, event_index: int | None) -> None:
    # fork_session and elone_session emit the identical payload shape,
    # distinguished only by event name, so one log-query shape covers both
    # spawn flows; the single helper is what keeps them agreeing.
    log.info(event, new_session=meta.id, parent=parent_id, event_index=event_index, backend=meta.backend)

  def get_chat_events_path(self, session_id: str) -> Path:
    """Return the absolute path to a session's chat_events.jsonl."""
    return self._chat_events.get_chat_events_path(session_id)

  def parent_reference_path(self, session_id: str) -> Path:
    """Return the absolute path to a session's parent_reference.jsonl."""
    return self._session_dir(session_id) / "data" / "parent_reference.jsonl"

  async def rename_session(self, session_id: str, new_name: str) -> SessionMetadata | None:
    """Rename a session and return the updated metadata."""
    return await self._update_field(session_id, "name", new_name, "session_renamed", new_name=new_name)

  async def switch_backend(self, session_id: str, backend: str) -> SessionMetadata | None:
    """Set the session's backend and return the updated metadata.

    Performs only the metadata write — the caller (API layer) is responsible
    for resume-domain validation and for persisting the audit event. Returns
    ``None`` when the session is missing. Broadcasts a sidebar update so other
    open tabs refresh their header.
    """
    meta = await self._update_field(session_id, "backend", backend, "session_backend_switched", backend=backend)
    if meta:
      await self._broadcast_sidebar(session_id, ET.BACKEND_SWITCHED, backend=backend)
    return meta

  async def mark_read(self, session_id: str) -> SessionMetadata | None:
    """Clear the unread flag for a session."""
    return await self._set_unread_flag(session_id, False)

  async def mark_unread(self, session_id: str) -> None:
    """Set the unread flag for a session (called when master/workers produce output)."""
    await self._set_unread_flag(session_id, True)

  async def _set_unread_flag(self, session_id: str, has_unread: bool) -> SessionMetadata | None:
    """Write the unread flag and broadcast only when it actually flips.

    Unlike ``_update_field`` this must not bump ``updated_at``: the sidebar
    sorts newest-first on that field, and a read/unread flip is not user
    activity worth reordering the list over.
    """
    async with self._lock_for(session_id):
      meta = await self.get_session(session_id)
      if not meta or meta.has_unread == has_unread:
        return meta
      meta.has_unread = has_unread
      await self.save_metadata(meta)
    await self._broadcast_sidebar(session_id, ET.UNREAD_CHANGED, has_unread=has_unread)
    return meta

  async def _broadcast_sidebar(self, session_id: str, event_type: str, **fields: Any) -> None:
    """Broadcast one session-scoped sidebar event.

    Every session-scoped sidebar event carries the same channel and
    ``session_id`` key; the helper is what keeps the senders agreeing on that
    payload shape.
    """
    await streaming_manager.broadcast("sidebar", {"type": event_type, "session_id": session_id, **fields})

  async def archive_session(self, session_id: str) -> SessionMetadata | None:
    """Mark a session as archived (does not delete files)."""
    meta = await self._update_field(session_id, "status", SessionStatus.ARCHIVED, "session_archived")
    self._drop_session_runtime_state(session_id)
    return meta

  async def delete_session_permanently(self, session_id: str) -> bool:
    """Permanently delete a session and all its data from disk."""
    async with self._lock_for(session_id):
      session_dir = self._session_dir(session_id)
      if not session_dir.exists():
        return False
      meta = await self.get_session(session_id)
      await self._backend_destroy_hook(session_id, meta)
      await asyncio.to_thread(shutil.rmtree, session_dir)
      self._drop_session_runtime_state(session_id)
      self._invalidate_cache(session_id)
      # Popping the lock from the dict while holding it is safe: the popped lock
      # object stays valid for this holder until the ``async with`` exits.
      self._metadata_locks.pop(session_id, None)
    log.info("session_deleted_permanently", session_id=session_id)
    return True

  async def unarchive_session(self, session_id: str) -> SessionMetadata | None:
    """Restore an archived session back to active."""
    return await self._update_field(session_id, "status", SessionStatus.ACTIVE, "session_unarchived")

  async def recycle_scheduled_session(self, session_id: str, cutoff_utc: datetime) -> dict:
    """GC old threads and archive old chat_events for a scheduled session.

    Threads whose status is completed/failed/cancelled and whose ``completed_at``
    is earlier than ``cutoff_utc`` are removed. Chat events with timestamp
    earlier than ``cutoff_utc`` are moved out of the live ``chat_events.jsonl``
    into a weekly archive file under ``data/archives/``. Best-effort per-thread
    and per-event-line: a single bad file is logged and skipped, not raised.
    """
    threads_deleted = await asyncio.to_thread(self._gc_old_threads_sync, session_id, cutoff_utc)
    archive_result = await asyncio.to_thread(
        self._chat_events.archive_old_chat_events_sync, session_id, cutoff_utc)
    events_archived = archive_result["events_archived"]
    archive_file = archive_result["archive_file"]

    if events_archived:
      async with self._lock_for(session_id):
        fresh = await self.get_session(session_id)
        if fresh is not None:
          fresh.archive_offset += events_archived
          await self.save_metadata(fresh)
      self._drop_session_runtime_state(session_id)

    log.info(
        "scheduled_session_recycle_done",
        session_id=session_id,
        threads_deleted=threads_deleted,
        events_archived=events_archived,
        archive_file=archive_file,
    )
    return {
        "threads_deleted": threads_deleted,
        "events_archived": events_archived,
        "archive_file": archive_file,
    }

  def _gc_old_threads_sync(self, session_id: str, cutoff_utc: datetime) -> int:
    """Remove thread dirs whose status is terminal and completed_at < cutoff."""
    threads_dir = self._threads_dir(session_id)
    if not threads_dir.exists():
      return 0
    deleted = 0
    for thread_dir in threads_dir.iterdir():
      try:
        if not thread_dir.is_dir():
          continue
        meta = load_json_meta(thread_dir / "metadata.json", "thread_meta_read_failed_during_recycle")
        if meta is None:
          continue
        if meta.get("status") not in TERMINAL_THREAD_STATUSES:
          continue
        completed_at = self._parse_optional_utc(
            meta.get("completed_at"), "thread_completed_at_parse_failed", thread=str(thread_dir))
        if completed_at is None or completed_at >= cutoff_utc:
          continue
        shutil.rmtree(thread_dir)
        deleted += 1
      except Exception:
        log.exception("thread_gc_failed", thread=str(thread_dir))
    return deleted

  async def star_session(self, session_id: str) -> SessionMetadata | None:
    """Star a session."""
    return await self._update_field(session_id, "starred", True, "session_starred")

  async def unstar_session(self, session_id: str) -> SessionMetadata | None:
    """Unstar a session."""
    return await self._update_field(session_id, "starred", False, "session_unstarred")

  async def set_group(self, session_id: str, group: str | None) -> SessionMetadata | None:
    """Set or clear the group for a session."""
    meta = await self._update_field(session_id, "group", group, "session_group_set")
    if meta:
      await self._broadcast_sidebar(session_id, ET.SESSION_GROUP_CHANGED, group=group)
    return meta

  async def rename_group(self, old_name: str, new_name: str) -> int:
    """Rename a group across all sessions. Returns the count of updated sessions."""
    count = await self._rewrite_group(old_name, new_name)
    if count:
      log.info("group_renamed", old_name=old_name, new_name=new_name, count=count)
    return count

  async def delete_group(self, group: str) -> int:
    """Remove a group from all sessions (set to null). Returns the count of updated sessions."""
    count = await self._rewrite_group(group, None)
    if count:
      log.info("group_deleted", group=group, count=count)
    return count

  async def _rewrite_group(self, old_name: str, new_name: str | None) -> int:
    """Set old_name's group to new_name on every matching session. Returns the count updated."""
    all_sessions = await self.list_sessions()
    count = 0
    for meta in all_sessions:
      if meta.group != old_name:
        continue
      async with self._lock_for(meta.id):
        fresh = await self.get_session(meta.id)
        if not fresh or fresh.group != old_name:
          continue
        fresh.group = new_name
        fresh.updated_at = utc_now()
        await self.save_metadata(fresh)
      count += 1
    return count

  async def persist_cc_session_id(self, session_id: str, cc_session_id: str) -> str | None:
    """Persist a cc_session_id without clobbering unrelated metadata fields.

    Re-reads fresh metadata from disk under the per-session lock, mutates only
    ``cc_session_id`` (and ``cc_session_started_at`` when the on-disk id actually
    changes), saves, then re-reads and returns the ``cc_session_id`` now on
    disk. Never falls back to a whole-object save — that clobbers concurrent
    single-field writes like ``has_unread``. ``update_thinking_state`` is the
    reference pattern.
    """
    async with self._lock_for(session_id):
      fresh = await self._get_session_bypassing_cache(session_id)
      if fresh is None:
        return None
      if fresh.cc_session_id != cc_session_id:
        fresh.cc_session_id = cc_session_id
        fresh.cc_session_started_at = utc_now()
      await self.save_metadata(fresh)
      read_back = await self._get_session_bypassing_cache(session_id)
    return read_back.cc_session_id if read_back is not None else None

  async def has_completed_round(self, session_id: str) -> bool:
    """True when the live event stream contains a master_done event.

    Reads through the existing ``load_chat_events_sync`` cache; adds no new
    persistent state.
    """
    events = self.load_chat_events_sync(session_id)
    return any(ev.get("type") == ET.MASTER_DONE for ev in events)

  async def persist_master_run(self, session_id: str, record: MasterRunRecord | None) -> None:
    """Set or clear the session's in-flight master-turn record.

    Same read-modify-write-under-lock pattern as ``persist_cc_session_id``: a
    whole-object save would clobber concurrent single-field writes.
    """
    async with self._lock_for(session_id):
      fresh = await self._get_session_bypassing_cache(session_id)
      if fresh is None:
        return
      fresh.master_run = record
      await self.save_metadata(fresh)

  async def update_thinking_state(
      self,
      session_id: str,
      updated_at: datetime,
  ) -> None:
    """Persist updated_at without clobbering unrelated fields.

    Re-reads fresh metadata from disk before writing, so concurrent changes
    to fields like 'group' are preserved.
    """
    async with self._lock_for(session_id):
      fresh = await self._get_session_bypassing_cache(session_id)
      if fresh:
        fresh.updated_at = updated_at
        await self.save_metadata(fresh)

  def list_active_session_metas(self) -> list[SessionMetadata]:
    """Return metadata for active sessions by reading metadata.json files.

    Sync method — returns full SessionMetadata objects so callers avoid
    a second disk read. Populates the metadata cache for every status as a
    side-effect: the boot-time recovery scan already reads each file, so the
    same pass warms the listing cache and archived entries stay authoritative
    from then on (``_fresh_cached_meta``).
    """
    if not self._cfg.sessions_dir.exists():
      return []
    results: list[SessionMetadata] = []
    now = time.monotonic()
    for d in self._cfg.sessions_dir.iterdir():
      if not d.is_dir():
        continue
      meta_path = self._metadata_path(d.name)
      if not meta_path.exists():
        continue
      try:
        raw = meta_path.read_text(encoding="utf-8")
        meta = SessionMetadata.model_validate_json(raw)
        self._metadata_cache[d.name] = (meta, now)
        if meta.status == SessionStatus.ACTIVE:
          results.append(_stamp_thinking_since(meta.model_copy()))
      except (OSError, ValueError) as e:
        log.debug("list_active_ids_skip", dir=d.name, error=str(e))
    return results

  # ---------------------------------------------------------------------------
  # Chat event persistence (NDJSON — for WebSocket catch-up)
  # ---------------------------------------------------------------------------

  async def save_chat_event(self, session_id: str, event: dict) -> None:
    """Append a single NDJSON event line to chat_events.jsonl."""
    await self._chat_events.save_chat_event(session_id, event)

  async def persist_and_broadcast(self, session_id: str, event: dict) -> None:
    """Persist event, run it through the session's aggregator, broadcast deltas + raw event.

    Raw events whose type is in ``_RAW_EVENTS_REPLACED_BY_DELTAS`` are no
    longer broadcast on the wire; the aggregator emits ``message``/``stream``
    deltas in their place. All other event types still flow as before because
    clients use them for state side-effects (e.g. ``master_done`` →
    stopThinking).
    """
    # Prime the events cache + aggregator before persisting so event_index
    # injection works on the very first call after server start (and so the
    # aggregator state matches what SSR/SPA-switch produced for the same
    # events).
    aggregator = self._get_or_init_aggregator(session_id)
    meta = await self.get_session(session_id)
    archive_offset = meta.archive_offset if meta else 0
    await self.save_chat_event(session_id, event)
    event["event_index"] = archive_offset + self._chat_events.cached_event_count(session_id) - 1

    channel = _session_channel(session_id)
    deltas = list(aggregator.feed(event))
    for delta in deltas:
      await streaming_manager.broadcast(channel, delta)
    if event.get("type") not in _RAW_EVENTS_REPLACED_BY_DELTAS:
      await streaming_manager.broadcast(channel, event)

    # Slack delivery hangs off the round's terminal event, after the broadcast
    # and in its own task: the funnel neither waits on Slack nor breaks when a
    # post fails, and a round re-attached after a restart delivers through this
    # same point. deliver_done decides for itself whether the round is one a
    # Slack thread is waiting for.
    if event.get("type") == ET.MASTER_DONE:
      from src.core.slack_listener import (
        # lazy: slack_listener imports SessionManager from this module at top level
        deliver_done,
      )

      create_logged_task(
          deliver_done(session_id, event, self._cfg, self), name=f"slack-deliver-{session_id}")

  async def broadcast_only(self, session_id: str, event: dict) -> None:
    """Broadcast an event on the session channel without persisting it as a chat event.

    Used for state-change notifications (e.g. ``plan_updated``) that must not
    pollute the chat history or replay on reconnect.
    """
    channel = _session_channel(session_id)
    await streaming_manager.broadcast(channel, event)

  def _get_or_init_aggregator(self, session_id: str) -> MessageAggregator:
    """Return the live aggregator for *session_id*, lazy-initialized from disk.

    On first use after server start, the aggregator catches up to the current
    on-disk state by silently consuming all persisted events. Their deltas are
    discarded -- subscribed clients have already rendered them via SSR or
    SPA-switch which both use the same aggregator logic.
    """
    aggregator = self._aggregators.get(session_id)
    if aggregator is not None:
      return aggregator
    # Live file only holds events from index archive_offset onward; seed the
    # aggregator's offset so the deltas it emits carry the same GLOBAL
    # event_index that persist_and_broadcast stamps on the raw event.
    aggregator = MessageAggregator(event_index_offset=self._chat_events.read_archive_offset_sync(session_id))
    for ev in self.load_chat_events_sync(session_id):
      for _ in aggregator.feed(ev):
        pass
    self._aggregators[session_id] = aggregator
    return aggregator

  def callbacks(self) -> SessionCallbacks:
    """Return a bundle of session-related callbacks for run_message()."""
    return SessionCallbacks(
        persist_and_broadcast=self.persist_and_broadcast,
        update_thinking_state=self.update_thinking_state,
        mark_unread=self.mark_unread,
        persist_cc_session_id=self.persist_cc_session_id,
        has_completed_round=self.has_completed_round,
        persist_master_run=self.persist_master_run,
    )

  def load_chat_events_sync(self, session_id: str) -> list[dict]:
    """Read all chat events for catch-up. Uses in-memory cache after first read."""
    return self._chat_events.load_chat_events_sync(session_id)

  def load_chat_events_tail(self, session_id: str, limit: int = 200) -> tuple[list[dict], int, bool]:
    """Load only the last *limit* events from disk. Does NOT populate _events_cache.

    Returns (events, total_line_count, has_more).
    """
    return self._chat_events.load_chat_events_tail(session_id, limit)

  def get_chat_event_count_sync(self, session_id: str, session_meta: SessionMetadata | None = None) -> int:
    """Return the current global chat event count without parsing event payloads."""
    return self._chat_events.get_chat_event_count_sync(session_id, session_meta)

  def load_chat_events_range(self, session_id: str, start: int, end: int) -> tuple[list[dict], bool]:
    """Load events in GLOBAL index range [start, end). Returns (events, has_more).

    Indices are global (archive_offset + line_in_live_file). When the requested
    range starts before the live file, archived chat_events files under
    ``data/archives/`` are read in chronological order to fill the gap.
    """
    return self._chat_events.load_chat_events_range(session_id, start, end)

  def get_message_projection(self, session_id: str) -> MessageProjection | None:
    """Return the memoized message-list projection for *session_id*.

    Lazily builds from ``events_to_view(load_chat_events_sync(session_id))``
    and caches per session (LRU, cap ``_PROJECTION_LRU_LIMIT``); appends
    advance the projection incrementally by atomically swapping in an
    advanced copy instead of rebuilding. Returns None when
    ``archive_offset != 0`` — those sessions fall back entirely to the
    event-index cursor path and never mix the two cursor domains.
    """
    if self._chat_events.read_archive_offset_sync(session_id) != 0:
      return None
    live = self.load_chat_events_sync(session_id)
    cached = self._projection_cache.get(session_id)
    if cached is not None and cached.event_count == len(live):
      self._projection_cache.move_to_end(session_id)
      return cached
    if cached is None or len(live) < cached.event_count:
      # A shrink means the live file was rewritten; the append-incremental
      # advance cannot roll state back, so only this path pays a full build.
      cached = MessageProjection(list(live))
    else:
      # Swapping one reference into the cache is atomic, and published
      # projections are immutable: concurrent advances race on copies and a
      # loser wastes work instead of corrupting shared state.
      cached = cached.advanced(live[cached.event_count:])
    self._projection_cache[session_id] = cached
    self._projection_cache.move_to_end(session_id)
    while len(self._projection_cache) > _PROJECTION_LRU_LIMIT:
      self._projection_cache.popitem(last=False)
    return cached

  def _drop_session_runtime_state(self, session_id: str) -> None:
    """Drop a session's live runtime state: chat-event cache, aggregator, projection, recap memo."""
    self._chat_events.clear_cache(session_id)
    self._aggregators.pop(session_id, None)
    self._projection_cache.pop(session_id, None)
    from src.core import recap  # lazy: recap imports SessionManager from this module

    recap.drop_extract_memo(session_id)

  async def resolve_session_usage(
      self,
      session_id: str,
      session_meta: SessionMetadata,
  ) -> dict | None:
    """Resolve display usage for a session view as a projection over its events.

    Usage is computed on demand from the full chat-event stream (no incremental
    cache). See ``src/core/session_usage.py`` for the tier contract.
    """
    return await self._session_usage.resolve_session_usage(session_id, session_meta)

  # ---------------------------------------------------------------------------
  # Private helpers
  # ---------------------------------------------------------------------------

  def _invalidate_cache(self, session_id: str) -> None:
    """Remove a session from the metadata cache."""
    self._metadata_cache.pop(session_id, None)

  def _fresh_cached_meta(self, session_id: str) -> SessionMetadata | None:
    """Return the cached metadata for *session_id* when the entry is authoritative.

    An archived entry is served regardless of age: archived metadata changes
    only through the in-process write funnel (``save_metadata`` refreshes the
    entry, ``delete_session_permanently`` invalidates it), so a TTL re-read
    buys nothing there and the archived set stays listable without disk scans.
    Active entries keep the ``_METADATA_CACHE_TTL`` freshness window. The two
    TTL-checked metadata readers (``get_session`` and ``_load_session_metas``)
    route through this one check, and a stale entry is evicted here, so the
    two cannot drift on freshness semantics. ``list_active_session_metas``
    reads metadata.json unconditionally and repopulates the cache from its own
    scan instead — a third reader this check does not govern.
    """
    cached = self._metadata_cache.get(session_id)
    if cached is None:
      return None
    meta, ts = cached
    if meta.status == SessionStatus.ARCHIVED:
      return meta
    if (time.monotonic() - ts) < _METADATA_CACHE_TTL:
      return meta
    del self._metadata_cache[session_id]
    return None

  @staticmethod
  def _parse_optional_utc(raw: Any, log_event: str, **log_ctx: Any) -> datetime | None:
    """Parse a stored timestamp tolerantly: absent or unparseable becomes None.

    Both scans that route through here (``_gc_old_threads_sync`` and
    ``pending_trigger_state_sync``) treat a bad timestamp as skip-and-continue,
    never as a hard failure, so one corrupt file must not abort the scan.
    """
    if not raw:
      return None
    try:
      return parse_utc_datetime(raw)
    except ValueError as e:
      log.debug(log_event, **log_ctx, error=str(e))
      return None

  @staticmethod
  def _migrate_round_rating_keys(meta: SessionMetadata) -> bool:
    """Rewrite pre-UUID rating keys from event_index strings to legacy ids."""
    if not meta.round_ratings:
      return False
    migrated = {}
    changed = False
    for key, value in meta.round_ratings.items():
      if key.isdigit():
        migrated[f"legacy:{key}"] = value
        changed = True
      else:
        migrated[key] = value
    if changed:
      meta.round_ratings = migrated
    return changed

  async def _load_session_metas(self, status: SessionStatus | None = None) -> list[SessionMetadata]:
    """Load session metadata, batching disk reads and parses for cache misses.

    Performs the listing preamble for the entry points routed through here
    (``list_sessions``, ``search_sessions``, and ``list_archived_page``):
    (1) return [] if sessions_dir does not exist, (2) list session directories
    under asyncio.to_thread to avoid blocking the event loop, (3) use fresh
    cache entries directly and read+parse all missing metadata files serially
    in one asyncio.to_thread call — the parse stays off the event loop —
    logging and dropping any session that fails to load. Returns the cached
    objects themselves, filtered to *status* when given: callers that hand
    metadata out of the manager copy through ``_iter_session_metas``. The sync
    active-only scan ``list_active_session_metas`` keeps its own per-file sync
    reads and does not route through here.
    """
    if not self._cfg.sessions_dir.exists():
      return []

    def _session_dir_names() -> list[str]:
      # DirEntry.is_dir() answers from the directory record itself on
      # d_type-aware filesystems, while Path.iterdir() rebuilds a Path per
      # entry and pays one stat() each: ~1 ms vs ~6 ms measured at ~1000
      # session dirs, per listing call.
      with os.scandir(self._cfg.sessions_dir) as entries:
        return [entry.name for entry in entries if entry.is_dir()]

    dir_names = await asyncio.to_thread(_session_dir_names)

    cached_metas: dict[str, SessionMetadata] = {}
    missing_ids: list[str] = []
    for session_id in dir_names:
      meta = self._fresh_cached_meta(session_id)
      if meta is None:
        missing_ids.append(session_id)
      else:
        cached_metas[session_id] = meta

    parsed_by_id: dict[str, SessionMetadata] = {}
    empty_ids: set[str] = set()
    load_failures: dict[str, Exception] = {}
    if missing_ids:
      def _read_and_parse_missing() -> None:
        for session_id in missing_ids:
          path = self._metadata_path(session_id)
          try:
            if not path.exists():
              continue
            raw = path.read_text(encoding="utf-8")
          except Exception as exc:
            load_failures[session_id] = exc
            continue
          if not raw.strip():
            empty_ids.add(session_id)
            continue
          try:
            parsed_by_id[session_id] = SessionMetadata.model_validate_json(raw)
          except Exception as exc:
            load_failures[session_id] = exc

      await asyncio.to_thread(_read_and_parse_missing)

    result: list[SessionMetadata] = []
    for session_id in dir_names:
      loaded_from_cache = session_id in cached_metas
      if loaded_from_cache:
        meta = cached_metas[session_id]
      else:
        if session_id in load_failures:
          log.warning("session_load_failed", session_id=session_id, error=str(load_failures[session_id]))
          continue
        if session_id in empty_ids:
          log.warning("session_metadata_empty", session_id=session_id, path=str(self._metadata_path(session_id)))
          continue
        if session_id not in parsed_by_id:
          continue
        meta = parsed_by_id[session_id]

      try:
        migrated = self._migrate_round_rating_keys(meta)
        if migrated:
          await self.save_metadata(meta)
      except Exception as exc:
        log.warning("session_load_failed", session_id=session_id, error=str(exc))
        continue

      if not loaded_from_cache and not migrated:
        self._metadata_cache.setdefault(session_id, (meta, time.monotonic()))
      if status is None or meta.status == status:
        result.append(meta)
    return result

  async def _iter_session_metas(self, status: SessionStatus | None = None) -> list[SessionMetadata]:
    """Copying wrapper over ``_load_session_metas``: the status filter runs first,
    so only metadata that leaves the manager pays the model_copy and thinking stamp."""
    return [_stamp_thinking_since(meta.model_copy()) for meta in await self._load_session_metas(status)]

  def _lock_for(self, session_id: str) -> asyncio.Lock:
    """Return (creating on first use) the per-session metadata RMW lock."""
    lock = self._metadata_locks.get(session_id)
    if lock is None:
      lock = asyncio.Lock()
      self._metadata_locks[session_id] = lock
    return lock

  async def _update_field(
      self, session_id: str, field: str, value: Any, log_event: str, **log_fields: Any
  ) -> SessionMetadata | None:
    """Get a session, set one field, save, and log. Returns None if session not found."""
    async with self._lock_for(session_id):
      meta = await self.get_session(session_id)
      if not meta:
        return None
      setattr(meta, field, value)
      meta.updated_at = utc_now()
      await self.save_metadata(meta)
    log.info(log_event, session_id=session_id, **log_fields)
    return meta

  async def _has_running_tasks(self, session_id: str) -> bool:
    """Check if a session has any thread currently marked 'running'.

    A running thread's metadata.json is recent by definition (written at start,
    never rewritten while it runs), so only threads whose metadata mtime is within
    RUNNING_SCAN_WINDOW are read+parsed; older thread dirs are dropped after a cheap
    scandir+stat with zero content reads, and a session whose only threads are old
    returns False without reading any metadata. Runs its filesystem work in a thread
    to keep the event loop responsive (called per-session by the sidebar/status polls).
    """
    return await asyncio.to_thread(has_running_tasks_sync, self._threads_dir(session_id))

  async def _enrich_and_sort(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
  ) -> list[SessionMetadata]:
    """Populate sidebar state and sort newest first."""
    await self.populate_sidebar_state(
        sessions,
        include_running_status=include_running_status,
        include_pending_trigger_status=include_pending_trigger_status,
        include_pending_plan_approval=include_pending_plan_approval,
    )
    sessions.sort(key=lambda s: s.updated_at, reverse=True)
    return sessions

  async def populate_sidebar_state(
      self,
      sessions: list[SessionMetadata],
      include_running_status: bool = False,
      include_pending_trigger_status: bool = False,
      include_pending_plan_approval: bool = False,
      force: bool = False,
  ) -> None:
    """Populate derived sidebar-only state on session metadata objects.

    Serves active sessions from the in-process sidebar snapshot
    (:mod:`src.core.sidebar_state`) with zero disk access, and re-probes — in
    ONE ``asyncio.to_thread`` task, serial over sessions, via the pure probe
    cores above — only the dirty sessions, or every active session on every
    10th call and whenever *force* is set (the ``/status?force=1`` escape
    hatch). A selected probe first checks the stat-only probe-input
    signature: unchanged inputs (never-probed excepted) skip the deep read in
    every case except an explicit *force*. A session with no snapshot entry
    (cold boot, new session) is probed like a dirty one, so an empty snapshot
    is a full probe. Archived sessions keep the constant-False shortcut.
    """
    if not sessions:
      return
    if not (include_running_status or include_pending_trigger_status or include_pending_plan_approval):
      return

    # Archived sessions cannot have running tasks or pending triggers, so skip
    # the per-session filesystem work for them.
    active_sessions = [m for m in sessions if m.status != SessionStatus.ARCHIVED]
    archived_sessions = [m for m in sessions if m.status == SessionStatus.ARCHIVED]

    for meta in archived_sessions:
      if include_running_status:
        meta.has_running_tasks = False
      if include_pending_trigger_status:
        meta.has_pending_trigger = False
        meta.pending_trigger_count = 0
        meta.next_trigger_at = None
      if include_pending_plan_approval:
        meta.has_pending_plan_approval = False

    if not active_sessions:
      return

    force_full = sidebar_state.register_poll(force)
    if force_full:
      probe_ids = [meta.id for meta in active_sessions]
    else:
      probe_ids = [
          meta.id for meta in active_sessions
          if sidebar_state.is_dirty(meta.id) or sidebar_state.snapshot_entry(meta.id) is None
      ]
    # Selection-time removal: a transition mark landing while the probe runs
    # re-adds the id, so that write is re-probed by the next poll even if the
    # in-flight probe raced it.
    sidebar_state.discard_dirty(probe_ids)

    if probe_ids:
      specs = [
          (session_id, self._threads_dir(session_id), self._session_dir(session_id) / "triggers",
           self._session_dir(session_id) / "plans.json")
          for session_id in probe_ids
      ]
      try:
        # Explicit force keeps its teeth as the escape hatch: it deep-probes
        # every selected session. Narrowed sweeps (the every-10th self-heal)
        # deep-probe only sessions whose probe-input signature moved.
        probed, probe_sigs = await asyncio.to_thread(selective_probe_sidebar_state, specs, deep=force)
      except BaseException:
        # A failed probe must not lose the dirty state it was serving.
        for session_id in probe_ids:
          sidebar_state.mark_sidebar_dirty(session_id)
        raise
      for session_id, entry in probed.items():
        sidebar_state.store_snapshot_entry(session_id, entry)
      for session_id, sig in probe_sigs.items():
        sidebar_state.store_probe_signature(session_id, sig)

    for meta in active_sessions:
      entry = sidebar_state.required_snapshot_entry(meta.id)
      if include_running_status:
        meta.has_running_tasks = bool(meta.thinking_since) or bool(entry["thread_running"])
      if include_pending_trigger_status:
        meta.has_pending_trigger = entry["pending_trigger_count"] > 0
        meta.pending_trigger_count = entry["pending_trigger_count"]
        meta.next_trigger_at = entry["next_trigger_at"]
      if include_pending_plan_approval:
        meta.has_pending_plan_approval = bool(entry["has_pending_plan_approval"])

  async def _next_session_name(self) -> str:
    """Generate 'Session 0', 'Session 1', etc. using a persistent counter file.

    Reads the next number from sessions_dir/.counter (O(1) instead of listing
    all sessions). Falls back to counting directories when the counter is
    missing, unreadable, or unparsable.
    """
    counter_path = self._cfg.sessions_dir / ".counter"

    def _read_and_increment() -> int:
      self._cfg.sessions_dir.mkdir(parents=True, exist_ok=True)
      # FileNotFoundError is an OSError, so a missing counter takes the same
      # count-dirs fallback as an unreadable or unparsable one; an exists()
      # pre-check would only open a check-then-read race.
      try:
        n = int(counter_path.read_text().strip())
      except (ValueError, OSError):
        n = self._count_session_dirs()
      counter_path.write_text(str(n + 1))
      return n

    n = await asyncio.to_thread(_read_and_increment)
    return f"Session {n}"

  def _count_session_dirs(self) -> int:
    """Count existing session directories for backward-compat counter init."""
    if not self._cfg.sessions_dir.exists():
      return 0
    return sum(1 for d in self._cfg.sessions_dir.iterdir() if d.is_dir())

  async def save_metadata(self, meta: SessionMetadata) -> None:
    """Persist *meta* to metadata.json and refresh the TTL cache from the serialized form.

    The write is atomic — a unique-per-call tmp file swapped in by ``os.replace``
    — and excludes ``_TRANSIENT_METADATA_FIELDS``. The cache entry stores the meta
    re-validated from that serialized form, so a cached read sees exactly what a
    disk read parses; that is what lets save-callers skip a manual cache populate
    (see ``get_session``'s migrate branch). ``updated_at`` is written as given:
    bumping or preserving it is the caller's decision (``_update_field`` bumps,
    ``_set_unread_flag`` does not).
    """
    path = self._metadata_path(meta.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    serialized = meta.model_dump_json(indent=2, exclude=_TRANSIENT_METADATA_FIELDS)

    await asyncio.to_thread(atomic_write_text, path, serialized)
    self._metadata_cache[meta.id] = (SessionMetadata.model_validate_json(serialized), time.monotonic())
    # The single funnel for every session-metadata write (35+ call sites, plus
    # the ScheduledSessionStore delegate): status transitions (archive/unarchive)
    # land here, so the sidebar snapshot must re-probe this session.
    sidebar_state.mark_sidebar_dirty(meta.id)

  def _session_dir(self, session_id: str) -> Path:
    return self._cfg.sessions_dir / session_id

  def _threads_dir(self, session_id: str) -> Path:
    """Return the absolute path to a session's threads dir."""
    return self._session_dir(session_id) / "threads"

  def _metadata_path(self, session_id: str) -> Path:
    return self._session_dir(session_id) / "metadata.json"
