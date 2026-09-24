"""Thread management API routes."""

import asyncio
import contextlib
import hashlib
import json
import os
import shlex
import threading
from collections.abc import Iterable
from dataclasses import dataclass
from datetime import UTC, datetime, timezone
from pathlib import Path

from fastapi import APIRouter, Depends, HTTPException, Query, Request, Response

from src.agents.backends.pty_common import (
  _TMUX_SOCKET,
  tmux_session_exists,
  tmux_session_name,
)
from src.api.deps import (
  get_config_on_loop,
  get_run_store,
  get_task_manager,
  get_thread_manager,
  get_trigger_manager,
  require_caller,
  task_manager,
)
from src.api.responses import (
    FastJsonResponse,
    fast_json_bytes,
    gzip_body_response,
)
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.constants import BackendType
from src.core.log_once import LazyStructlogLogger
from src.core.memo import BoundedMemo, StatSignatureMemo
from src.core.message_aggregator import (
    TOOL_PREVIEW_CHARS,
    extract_text_from_message,
    extract_tool_result_text,
    tool_preview,
)
from src.core.models import (
    CcClaudeBackend,
    PendingTrigger,
    RunRecord,
    ThreadMetadata,
    ThreadStatus,
    TuiCliBackend,
    WorkerEvent,
)
from src.core.ndjson import PARSE_SKIP_LOG_EVENT, iter_ndjson_events
from src.core.process import kill_process_group
from src.core.run_token import CallerIdentity
from src.core.runs import RunIdentityConflictError, RunNotFoundError
from src.core.sidebar_state import RevisionSweepGate, session_revision, take_marked_paths
from src.core.threads import METADATA_NAME, THREADS_DIR_NAME, ThreadManager, iter_thread_meta_stats
from src.core.triggers import TriggerManager, iter_trigger_file_stats

log = LazyStructlogLogger()

router = APIRouter()

# Cap on the description prefix shipped in the workers-panel list rows; the
# card paints one CSS-truncated line (overflow hidden + ellipsis), so 100 chars
# — a text-sm line at ~700 px — already exceeds what any width shows. Longer
# text reaches the modal through the description_full_len click-fetch.
_LIST_DESCRIPTION_CAP = 100

# One wire sentence for every thread-missing 404, whatever the endpoint raised it.
_THREAD_NOT_FOUND_DETAIL = "Thread not found"

# Parsed-meta memo behind the thread-detail endpoint. The endpoint reads the
# meta without mutating it, so the memoized instance is shared read-only
# (mutating callers go through ThreadManager.get_thread, which re-reads). Every
# writer publishes metadata.json through the atomic tmp rename, so a content
# change always moves (mtime_ns, size). Stat-before-read is StatSignatureMemo's
# contract.
_DETAIL_META_MEMO_LIMIT = 32
_detail_meta_memo: StatSignatureMemo[str, ThreadMetadata] = StatSignatureMemo(_DETAIL_META_MEMO_LIMIT)


async def _detail_thread_meta(thread_mgr: ThreadManager, session_id: str, thread_id: str) -> ThreadMetadata | None:
  path = thread_mgr.thread_dir(session_id, thread_id) / METADATA_NAME
  key = str(path)
  try:
    st = os.stat(path)
  except OSError:
    _detail_meta_memo.drop(key)
    return None
  meta = _detail_meta_memo.fresh(key, st)
  if meta is not None:
    return meta
  meta = await thread_mgr.get_thread(session_id, thread_id)
  if meta is None:
    return None
  _detail_meta_memo.record(key, st, meta)
  return meta


@dataclass(frozen=True)
class _BackendDispatch:
  type: str
  cli_binary: str | None = None


def _backend_dispatch(thread: ThreadMetadata, cfg: CharlieBotConfig | None) -> _BackendDispatch | None:
  if not thread.backend:
    return None
  if cfg is not None:
    option = cfg.get_backend_option(thread.backend)
    if option is not None:
      # cli_binary is declared on the cc-claude and tui-cli option models only;
      # every other member of the discriminated union must read None — a bare
      # attribute read raises AttributeError on pydantic's extra='forbid' models.
      return _BackendDispatch(
          type=option.type,
          cli_binary=option.cli_binary if isinstance(option, (CcClaudeBackend, TuiCliBackend)) else None)
  return _BackendDispatch(type=thread.backend)


def _tmux_attach_command(session_id: str, *, read_only: bool = False) -> str:
  command = ["tmux", "-L", _TMUX_SOCKET, "attach"]
  if read_only:
    command.append("-r")
  command.extend(["-t", tmux_session_name(session_id)])
  return shlex.join(command)


def _tmux_attach_id(thread: ThreadMetadata, dispatch: _BackendDispatch) -> str | None:
  if dispatch.type == BackendType.TUI_CLI:
    return thread.session_id
  if dispatch.type == BackendType.CC_CLAUDE and dispatch.cli_binary == "claude-sub":
    return thread.claude_session_id
  return None


def build_attach_command(thread: ThreadMetadata, cfg: CharlieBotConfig | None = None) -> str | None:
  dispatch = _backend_dispatch(thread, cfg)
  if dispatch is None:
    return None

  if dispatch.type == BackendType.CC_CLAUDE:
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id:
      return _tmux_attach_command(tmux_id, read_only=dispatch.cli_binary == "claude-sub")
    if not thread.worktree_path or not thread.claude_session_id:
      return None
    return f"cd {shlex.quote(thread.worktree_path)} && claude --resume {shlex.quote(thread.claude_session_id)}"
  if dispatch.type == BackendType.TUI_CLI:
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id is None:
      return None
    return _tmux_attach_command(tmux_id)
  return None


async def _attach_available(thread: ThreadMetadata, cfg: CharlieBotConfig) -> bool:
  dispatch = _backend_dispatch(thread, cfg)
  if dispatch is None:
    return False

  if dispatch.type == BackendType.CC_CLAUDE:
    tmux_id = _tmux_attach_id(thread, dispatch)
    if tmux_id:
      return await tmux_session_exists(tmux_id)
    return bool(thread.claude_session_id and thread.worktree_path and os.path.isdir(thread.worktree_path))
  if dispatch.type == BackendType.TUI_CLI:
    tmux_id = _tmux_attach_id(thread, dispatch)
    return bool(tmux_id and await tmux_session_exists(tmux_id))
  return False


def _epoch_ms(dt: datetime) -> int:
  """The UTC timestamp as epoch milliseconds, the wire form of the list row timestamps."""
  return int(dt.timestamp() * 1000)


def _v2_run_status(run: RunRecord, events: list[dict], host_boot: datetime) -> str:
  """The legacy status string one Run's facts map to (the compat row's contract).

  Terminal facts win; a queued run is idle (cancelled once a durable stop
  request stands); a launched run is running while its process identity is
  live and failed once the process is gone without a terminal fact (the
  recovery stage owns the final resolve).
  """
  from src.core.runs import is_run_alive
  outcome = run_store_outcome(events, run.id)
  if outcome == "success":
    return "completed"
  if outcome is not None:
    return "failed"
  if run.pid is None:
    return "cancelled" if run_store_stop_requested(events, run.id) else "idle"
  return "running" if is_run_alive(run.pid, run.pid_start, run.started_at, host_boot) else "failed"


def run_store_outcome(events: list[dict], run_id: str) -> str | None:
  """The run_finished outcome of one Run from the fact history (None while none)."""
  outcome = None
  for event in events:
    if event.get("type") == ET.RUN_FINISHED and event.get("run_id") == run_id:
      outcome = event.get("outcome")
  return outcome


def run_store_stop_requested(events: list[dict], run_id: str) -> bool:
  """Whether a durable run_stop_requested fact exists for one Run."""
  return any(
      event.get("type") == ET.RUN_STOP_REQUESTED and event.get("run_id") == run_id
      for event in events)


def _v2_run_list_item(
    run: RunRecord,
    events: list[dict],
    host_boot: datetime,
    *,
    created_at: datetime,
    description: str,
    branch_name: str | None = None,
    worktree_path: str | None = None,
    pid: int | None = None,
) -> dict:
  """One ephemeral compatibility row for a v2 Run (no ThreadMetadata is written).

  The row carries the actual run identity — its own id, backend/model, pid and
  derived status — so a legacy consumer listing, reading or stopping threads
  addresses the same Run the v2 routes serve.
  """
  item = {
      "type": "thread",
      "id": run.id,
      "description": description[:_LIST_DESCRIPTION_CAP],
      "status": _v2_run_status(run, events, host_boot),
      "created_at": _epoch_ms(created_at),
      "completed_at": _epoch_ms(run.ended_at) if run.ended_at else None,
      "backend": run.backend,
      "session_id": run.session_id,
  }
  if len(description) > _LIST_DESCRIPTION_CAP:
    item["description_full_len"] = len(description)
  if pid is not None:
    item["pid"] = pid
  if branch_name:
    item["branch_name"] = branch_name
  if worktree_path:
    item["worktree_path"] = worktree_path
  return item


def _thread_list_item(t: ThreadMetadata) -> dict:
  """One thread row of the workers-panel list and session-view payloads.

  The description ships as a prefix: the card it backs paints one CSS-truncated
  line and the full-text modal fetches the thread row on click, so neither
  payload ships task-spec-length descriptions (~KB each). A truncated row
  carries ``description_full_len`` so the client knows to fetch. Timestamps
  ship as epoch milliseconds: the client reads both fields through ``new
  Date()``, which accepts the integer and the ISO string alike, and the int
  form halves their wire bytes on a body that scales with the session's thread
  count.
  """
  description = t.description or ""
  item = {
      "type": "thread",
      "id": t.id,
      "description": description[:_LIST_DESCRIPTION_CAP],
      "status": t.status.value,
      "created_at": _epoch_ms(t.created_at),
      "completed_at": _epoch_ms(t.completed_at) if t.completed_at else None,
      "backend": t.backend,
  }
  if len(description) > _LIST_DESCRIPTION_CAP:
    item["description_full_len"] = len(description)
  return item


# Whole-body memo for the 3 s workers-panel list poll: body bytes per session
# keyed on the union file signature. Every row field derives from thread
# metadata.json and trigger *.json files, and every writer rewrites those
# files atomically (a rename always moves mtime_ns), so an unchanged signature
# proves the built body is still current. Single slot per session with an LRU
# cap: one slot holds the worst body (its bytes scale with the session's
# thread count), and deeper caps buy nothing because a session's poll reuses
# its one slot.
_LIST_BODY_MEMO_LIMIT = 8
_list_body_memo: BoundedMemo[str, tuple[tuple[tuple[str, int, int], ...], bytes,
                                        str]] = BoundedMemo(_LIST_BODY_MEMO_LIMIT)

# The list poll's gzip form rides the body-keyed memo: the rendered body bytes
# are their own invalidation ground (a memo hit proves byte equality because
# the dict key IS the body), so one off-loop level-1 deflate per distinct body
# replaces the middleware's per-request pass; Content-Encoding set upstream
# makes the middleware skip (the M72 mechanism). The key shares the lifetime
# rule the body memo's signature proves, so the same LRU cap fits.
_LIST_GZIP_MEMO_LIMIT = 8
_list_gzip_memo: BoundedMemo[bytes, bytes] = BoundedMemo(_LIST_GZIP_MEMO_LIMIT)

# The events full fetch's gzip form, keyed on the body bytes themselves. One
# slot per distinct projection: a log append moves the body, so the cap bounds
# the memo at the worst body's bytes, and a repeat open of an unchanged log
# serves the stored bytes instead of the middleware's per-request deflate.
_EVENTS_GZIP_MEMO_LIMIT = 8
_events_gzip_memo: BoundedMemo[bytes, bytes] = BoundedMemo(_EVENTS_GZIP_MEMO_LIMIT)

# The thread-detail poll's gzip form rides the same body-keyed memo: the full
# row's 50 KB body re-deflates inside the middleware on every served request
# although the rendered bytes are their own invalidation ground. One off-loop
# level-1 deflate per distinct body replaces it; Content-Encoding set upstream
# makes the middleware skip (the M72 mechanism).
_DETAIL_GZIP_MEMO_LIMIT = 8
_detail_gzip_memo: BoundedMemo[bytes, bytes] = BoundedMemo(_DETAIL_GZIP_MEMO_LIMIT)

# Polls between signature walks, per session. Every writer of a row-source
# file (thread metadata via _save_metadata, triggers via _save_trigger) marks
# through mark_sidebar_dirty, so an unchanged session_revision proves the
# stored signature still describes the files and the memo serves without the
# per-file stat walk. The sweep walk every Nth poll bounds a mark its writer
# path forgot to the same ~30 s window the sidebar's populate sweep accepts.
_LIST_PROOF_SWEEP_EVERY = 10
_sig_gate = RevisionSweepGate(_LIST_PROOF_SWEEP_EVERY)


def _row_source_stats(
    threads_dir: str,
    triggers_dir: str,
    runs_dir: str | None = None,
) -> tuple[list[tuple[str, os.stat_result]], list[tuple[str, os.stat_result]],
           list[tuple[str, os.stat_result]]]:
  """One scandir+stat walk of the row-source directories, split by directory.

  The list body's freshness signature and its thread rows read the same files,
  so a rebuild walks once and feeds both; two walks would stat every
  metadata.json twice per rebuild. A directory that cannot be scanned
  contributes an empty half, the same "no rows" verdict the signature's
  OSError swallow gives it. The third source is the task tree's Run metadata
  files: the v2 compatibility rows ride the same proof, so a Run's metadata
  write (atomic rename, mtime moves) refreshes its row inside the same sweep
  window the unmarked thread write heals in.
  """
  thread_pairs: list[tuple[str, os.stat_result]] = []
  with contextlib.suppress(OSError):
    thread_pairs.extend(iter_thread_meta_stats(threads_dir))
  trigger_pairs: list[tuple[str, os.stat_result]] = []
  with contextlib.suppress(OSError):
    trigger_pairs.extend(iter_trigger_file_stats(triggers_dir))
  run_pairs: list[tuple[str, os.stat_result]] = []
  if runs_dir:
    try:
      for entry in os.scandir(runs_dir):
        if not entry.is_dir():
          continue
        meta_path = os.path.join(entry.path, "metadata.json")
        try:
          run_pairs.append((meta_path, os.stat(meta_path)))
        except OSError:
          continue
    except OSError:
      pass
  return thread_pairs, trigger_pairs, run_pairs


def _signature_from_stats(
    thread_pairs: list[tuple[str, os.stat_result]],
    trigger_pairs: list[tuple[str, os.stat_result]],
    run_pairs: list[tuple[str, os.stat_result]] = (),
) -> tuple[tuple[str, int, int], ...]:
  """(path, mtime_ns, size) of every row-source file, in the memo's sorted-key order."""
  sig = [(path, st.st_mtime_ns, st.st_size) for path, st in thread_pairs]
  sig.extend((path, st.st_mtime_ns, st.st_size) for path, st in trigger_pairs)
  sig.extend((path, st.st_mtime_ns, st.st_size) for path, st in run_pairs)
  sig.sort()
  return tuple(sig)


# Built list rows per session: metadata.json path -> (mtime_ns, size, row, fragment).
# _thread_list_item is a pure function of the parsed metadata and the parse
# memo keys the same (mtime_ns, size) identity every atomic rename moves, so
# an unchanged stat proves the stored row current and a marked rebuild rebuilds
# only the moved files' rows. Rows are shared read-only into the body payload,
# and the fragment is the row's rendered JSON: the list body assembles from
# fragments (the M72 files-listing row-memo shape) because a changed poll would
# otherwise re-dump every unmoved row — 0.60 ms of stdlib dumps per rebuild on
# the 339-row worst corpus against 0.01 ms of join.
_THREAD_ROW_MEMO_LIMIT = 8
_thread_row_memo: BoundedMemo[str, dict[str, tuple[int, int, dict, bytes]]] = BoundedMemo(_THREAD_ROW_MEMO_LIMIT)

# The one home of the list body's JSON options. The encoder's per-element text
# is context-free, so a row's standalone rendering is byte-identical to its
# rendering inside the whole-body array dump (the _EventBatcher property in
# trace_merge); the joined fragments and the whole dump are pinned equal by
# test_list_body_splice_matches_whole_dump.
_ROW_DUMPS_OPTS = {"ensure_ascii": False, "allow_nan": False, "separators": (",", ":")}


def _row_fragment(row: dict) -> bytes:
  """Render one row's JSON fragment with the list body's options."""
  return json.dumps(row, **_ROW_DUMPS_OPTS).encode("utf-8")


async def _v2_run_list_items(
    session_id: str,
    run_pairs: list[tuple[str, os.stat_result]],
) -> list[tuple[dict, bytes]]:
  """Compatibility (row, fragment) pairs for the session's v2 Runs, from the walked metadata files.

  A session without task-tree runs (every v1 session) scans zero directories
  and costs nothing beyond the empty scandir. Rows derive from the Run record
  plus the fact history — no ThreadMetadata file is read or written; the row
  memo keys the run metadata files the same (mtime_ns, size) identity the
  signature proves, and the fragment rides the memo entry beside the row the
  same way _thread_list_items stores its pairs.
  """
  if not run_pairs:
    return []
  tree = task_manager()
  meta = await tree.load_meta(session_id)
  description = (meta.task.goal if meta is not None and meta.task is not None else "") or ""
  events = tree.runs.load_events_sync(session_id)

  refreshed: dict[str, tuple[int, int, dict, bytes]] = {}
  hit = _thread_row_memo.get(session_id)
  items: list[tuple[dict, bytes]] = []
  for meta_path, st in run_pairs:
    run_id = Path(meta_path).parent.name
    cached = hit.get(meta_path) if hit is not None else None
    if cached is not None and cached[0] == st.st_mtime_ns and cached[1] == st.st_size:
      items.append((cached[2], cached[3]))
      refreshed[meta_path] = cached
      continue
    run = await asyncio.to_thread(tree.runs.read_run_sync, session_id, run_id)
    if run is None:
      continue
    created_at = run.started_at or datetime.fromtimestamp(st.st_mtime, tz=timezone.utc)
    item = _v2_run_list_item(
        run, events, datetime.now(UTC),
        created_at=created_at,
        description=description,
        branch_name=run.branch_name,
        worktree_path=run.worktree_path,
        pid=run.pid,
    )
    fragment = _row_fragment(item)
    refreshed[meta_path] = (st.st_mtime_ns, st.st_size, item, fragment)
    items.append((item, fragment))
  _thread_row_memo.store(session_id, refreshed)
  return items


def _thread_list_items(
    session_id: str, thread_pairs: list[tuple[str, os.stat_result]],
    metas: list[ThreadMetadata | None]) -> list[tuple[dict, bytes]]:
  """List (row, fragment) pairs for the walked pairs, served from the row memo where the file stands.

  *metas* aligns position-for-position with *thread_pairs*; a ``None`` entry is
  the parse-miss verdict for a file that vanished between the walk and its
  read, so it gets no row — exactly as if the walk's stat had failed.
  """
  refreshed: dict[str, tuple[int, int, dict, bytes]] = {}
  items = []
  for (meta_path, st), meta in zip(thread_pairs, metas, strict=True):
    if meta is None:
      continue
    hit = _thread_row_memo.get(session_id)
    cached = hit.get(meta_path) if hit is not None else None
    if cached is not None and cached[0] == st.st_mtime_ns and cached[1] == st.st_size:
      item, fragment = cached[2], cached[3]
    else:
      item = _thread_list_item(meta)
      fragment = _row_fragment(item)
    refreshed[meta_path] = (st.st_mtime_ns, st.st_size, item, fragment)
    items.append((item, fragment))
  _thread_row_memo.store(session_id, refreshed)
  return items


def _trigger_list_item(tr: PendingTrigger) -> dict:
  """One trigger row of the workers-panel list payload.

  Timestamps ride the same epoch-ms wire form as the thread rows: ``_list_body``
  sorts both row kinds by ``created_at``, so the mixed sort stays homogeneous.
  The client's first paint reads the raw JSON value through ``new Date()``,
  which accepts the integer; its poll-update path re-reads ``fire_at`` from the
  ``data-fire-at`` attribute as a string, where only the numeric form coerces —
  the card's formatter handles that coercion (``formatTriggerTimeLabel``).
  """
  return {
      "type": "trigger",
      "id": tr.id,
      "message": tr.message,
      "status": tr.status.value,
      "fire_at": _epoch_ms(tr.fire_at),
      "created_at": _epoch_ms(tr.created_at),
  }


def _list_body(rows: list[tuple[dict, bytes]], triggers: list[PendingTrigger]) -> bytes:
  """The list body from (row, fragment) pairs plus trigger rows: the combined sort, then the fragment join.

  The body is the fragments joined inside array brackets, not a whole-array
  dumps — a changed poll would otherwise re-encode every unmoved row (the
  encoder's per-element text is context-free, so the join is byte-identical;
  pinned by test_list_body_splice_matches_whole_dump).
  """
  combined = list(rows)
  for tr in triggers:
    item = _trigger_list_item(tr)
    combined.append((item, _row_fragment(item)))
  combined.sort(key=lambda pair: pair[0]["created_at"], reverse=True)
  return b"[" + b",".join(fragment for _item, fragment in combined) + b"]"


async def _marked_rebuild(
    session_id: str,
    session_dir: Path,
    hit: tuple[tuple[tuple[str, int, int], ...], bytes, str],
    marked: list[str],
    trigger_mgr: TriggerManager,
) -> tuple[tuple[tuple[str, int, int], ...], bytes, str] | None:
  """Prove the stored list body against exactly the marked row-source files.

  The writers mark the file they just published through their atomic rename,
  so stat-ing the marked paths and leaving every other signature entry standing
  proves the body the way a full walk would, at one stat per mark. Returns the
  new (sig, body, etag), or None when the marked shape cannot prove
  incrementally — the row memo evicted this session, or a marked path is not a
  thread metadata file — and the caller falls back to the full walk.
  """
  rows = _thread_row_memo.get(session_id)
  if rows is None:
    return None
  threads_prefix = str(session_dir / THREADS_DIR_NAME) + "/"
  sig_entries = {path: (mtime, size) for path, mtime, size in hit[0]}
  refreshed: dict[str, tuple[int, int, dict, bytes]] = dict(rows)
  moved = False
  for path in marked:
    try:
      st = os.stat(path)
    except OSError:
      sig_entries.pop(path, None)
      refreshed.pop(path, None)
      moved = True
      continue
    cached = rows.get(path)
    if cached is not None and cached[0] == st.st_mtime_ns and cached[1] == st.st_size:
      continue  # a repeat mark whose file already stands in the proof: nothing moved
    if not path.startswith(threads_prefix):
      return None
    try:
      with open(path, encoding="utf-8") as f:
        meta = ThreadMetadata.model_validate_json(f.read())
    except OSError:
      # Stat saw the file, so the read failure means it vanished between stat
      # and read: the no-row verdict the full walk gives, entry and row gone.
      sig_entries.pop(path, None)
      refreshed.pop(path, None)
      moved = True
      continue
    item = _thread_list_item(meta)
    refreshed[path] = (st.st_mtime_ns, st.st_size, item, _row_fragment(item))
    sig_entries[path] = (st.st_mtime_ns, st.st_size)
    moved = True
  sig = tuple(sorted((path, mtime, size) for path, (mtime, size) in sig_entries.items()))
  _thread_row_memo.store(session_id, refreshed)
  if not moved:
    # Every marked file already stands in the proof (a repeat mark): the
    # stored body is current, serve it instead of rebuilding the same bytes.
    return hit
  triggers = await trigger_mgr.list_triggers(session_id)
  body = _list_body([(entry[2], entry[3]) for entry in refreshed.values()], triggers)
  etag_value = '"' + hashlib.sha1(body).hexdigest() + '"'
  return sig, body, etag_value


async def _list_response(request: Request, body: bytes, etag_value: str, etag: str | None) -> Response:
  """The list body's answer: a bodyless 204 when the poll repeats the rendered tag."""
  if etag == etag_value:
    return Response(status_code=204, headers={"ETag": etag_value, "Cache-Control": "no-store"})
  return await gzip_body_response(request, body, {"ETag": etag_value, "Cache-Control": "no-store"}, _list_gzip_memo)


# The session view's threads array rides the same row proof as the list body:
# sorted rows per session gated on the write revision (every row-source
# writer marks through mark_sidebar_dirty). The view itself writes nothing
# (the read mark rides the client's post-render POST /read),
# so repeat views serve rows with zero stats; a writer mark or the sweep walk
# rebuilds from the walked pairs, the row memo serving the unmoved files'
# rows.
_VIEW_ROWS_MEMO_LIMIT = 8
_VIEW_ROWS_SWEEP_EVERY = 10
_view_rows_memo: BoundedMemo[str, list[dict]] = BoundedMemo(_VIEW_ROWS_MEMO_LIMIT)
_view_rows_gate = RevisionSweepGate(_VIEW_ROWS_SWEEP_EVERY)


async def view_thread_rows(
    session_id: str,
    cfg: CharlieBotConfig,
    thread_mgr: ThreadManager,
) -> list[dict]:
  """Thread rows for the session view payload, proven current like the list body.

  Serves the stored rows while the session's write revision stands (bounded by
  the same sweep window as the list poll); a mark or the sweep walks the
  row-source directories once and rebuilds through the shared row memo. Rows
  are shared read-only with the workers-panel list's row memo and sorted
  newest-first, the list_threads order.
  """
  hit = _view_rows_memo.get(session_id)
  rev = session_revision(session_id)
  if hit is not None and _view_rows_gate.serve_hit(session_id, rev):
    return hit
  session_dir = cfg.sessions_dir / session_id

  def walk_and_parse() -> tuple[list[tuple[str, os.stat_result]], list[tuple[str, os.stat_result]], list[ThreadMetadata | None]]:
    thread_pairs, _triggers, run_pairs = _row_source_stats(
        str(session_dir / THREADS_DIR_NAME), str(session_dir / "triggers"),
        str(session_dir / "data" / "runs"))
    return thread_pairs, run_pairs, thread_mgr.list_threads_from_stats(thread_pairs)

  thread_pairs, run_pairs, metas = await asyncio.to_thread(walk_and_parse)
  rows = [row for row, _fragment in _thread_list_items(session_id, thread_pairs, metas)]
  rows.extend(row for row, _fragment in await _v2_run_list_items(session_id, run_pairs))
  rows.sort(key=lambda row: row["created_at"], reverse=True)
  _view_rows_memo.store(session_id, rows)
  _view_rows_gate.mark_proven(session_id, rev)
  return rows


@router.get("/{session_id}/list")
async def list_threads(
    request: Request,
    session_id: str,
    etag: str | None = Query(default=None),
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    trigger_mgr: TriggerManager = Depends(get_trigger_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
) -> Response:
  """Return mixed list of thread and trigger summaries, sorted by created_at descending.

  The body carries a strong ETag (sha1 of the body bytes). A poll repeating the
  ETag it rendered via ``?etag=`` is answered 204 with no body. The conditional
  rides a query param rather than If-None-Match because the browser's HTTP
  cache fulfils a revalidation itself and fetch never surfaces the 304; the
  no-store on every answer keeps each poll a real request.
  """
  session_dir = cfg.sessions_dir / session_id
  hit = _list_body_memo.get(session_id)
  rev = session_revision(session_id)
  # Drained every request so marks never pile up; a mark that landed between
  # the revision read above and this take left the revision bumped, so the next
  # poll full-walks (marked_since_proof with no paths) and catches its write.
  marked = take_marked_paths(session_id)
  thread_pairs: list[tuple[str, os.stat_result]] | None = None
  if hit is not None and _sig_gate.serve_hit(session_id, rev):
    sig = hit[0]
  else:
    marked_body = None
    if hit is not None and marked and _sig_gate.marked_since_proof(session_id, rev):
      marked_body = await _marked_rebuild(session_id, session_dir, hit, marked, trigger_mgr)
    if marked_body is not None:
      sig, body, etag_value = marked_body
      _list_body_memo.store(session_id, (sig, body, etag_value))
      # The incremental proof covered only the marked files: reset_sweep=False
      # advances the countdown, so the full walk still arrives on schedule and
      # an unmarked row-source write heals inside the same ~30 s window.
      _sig_gate.mark_proven(session_id, rev, reset_sweep=False)
      return await _list_response(request, body, etag_value, etag)
    thread_pairs, trigger_pairs, run_pairs = await asyncio.to_thread(
        _row_source_stats, str(session_dir / THREADS_DIR_NAME), str(session_dir / "triggers"),
        str(session_dir / "data" / "runs"))
    sig = _signature_from_stats(thread_pairs, trigger_pairs, run_pairs)
    if hit is not None and hit[0] == sig:
      _sig_gate.mark_proven(session_id, rev)
    else:
      _sig_gate.drop(session_id)
  if hit is not None and hit[0] == sig:
    return await _list_response(request, hit[1], hit[2], etag)

  # The rebuild's rows parse from the same walked pairs the signature keys, so
  # the memo's proof and the rows behind the body describe one instant. The
  # gate-hit path always serves above, so a rebuild implies the walk ran.
  assert thread_pairs is not None
  metas = await asyncio.to_thread(thread_mgr.list_threads_from_stats, thread_pairs)
  thread_items = _thread_list_items(session_id, thread_pairs, metas)
  thread_items.extend(await _v2_run_list_items(session_id, run_pairs))

  triggers = await trigger_mgr.list_triggers(session_id)
  # The marked rebuild's body rides this same _list_body build.
  body = _list_body(thread_items, triggers)
  etag_value = '"' + hashlib.sha1(body).hexdigest() + '"'
  _list_body_memo.store(session_id, (sig, body, etag_value))
  _sig_gate.mark_proven(session_id, rev)
  return await _list_response(request, body, etag_value, etag)


@router.get("/{session_id}/threads/{thread_id}")
async def get_thread(
    session_id: str,
    thread_id: str,
    request: Request,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    cfg: CharlieBotConfig = Depends(get_config_on_loop),
    attach: bool = Query(default=False),
) -> Response:
  """Return a thread's metadata plus the derived attach pair.

  With ``attach`` the response is only ``{"attach_command", "attach_available"}``
  — the 5 s poll's shape (the poll reads nothing else of the row). Without it
  the response is the full row minus ``context`` (the task-spec body; no HTTP
  consumer reads it off this endpoint, and the modal fetches ``description``
  once per click).
  """
  v2_run = await _resolve_v2_run(session_id, thread_id)
  if v2_run is not None:
    # The alias resolves to the same Run the v2 routes serve: the detail row
    # shows its actual running/finished identity, and no second thread write
    # exists behind it.
    run = await asyncio.to_thread(task_manager().runs.read_run_sync, v2_run[0], v2_run[1])
    if run is None:
      raise HTTPException(status_code=404, detail=_THREAD_NOT_FOUND_DETAIL)
    if attach:
      return FastJsonResponse({"attach_command": None, "attach_available": False})
    tree_meta = await task_manager().load_meta(v2_run[0])
    description = (tree_meta.task.goal if tree_meta is not None and tree_meta.task is not None else "") or ""
    events = task_manager().runs.load_events_sync(v2_run[0])
    row = _v2_run_list_item(
        run, events, datetime.now(UTC),
        created_at=run.started_at or datetime.now(UTC),
        description=description,
        branch_name=run.branch_name,
        worktree_path=run.worktree_path,
        pid=run.pid,
    )
    row["session_id"] = v2_run[0]
    row["description_full"] = description
    return FastJsonResponse(row)
  meta = await _detail_thread_meta(thread_mgr, session_id, thread_id)
  if not meta:
    raise HTTPException(status_code=404, detail=_THREAD_NOT_FOUND_DETAIL)
  attach_command = build_attach_command(meta, cfg)
  attach_available = await _attach_available(meta, cfg)
  if attach:
    return FastJsonResponse({
        "attach_command": attach_command,
        "attach_available": attach_available,
    })
  payload = meta.model_dump(mode="json")
  del payload["context"]
  payload["attach_command"] = attach_command
  payload["attach_available"] = attach_available
  return await gzip_body_response(request, fast_json_bytes(payload), {}, _detail_gzip_memo)


async def _resolve_v2_run(owner_session_id: str, thread_id: str) -> tuple[str, str] | None:
  """Resolve a legacy thread address to its owning v2 (session, run), if any.

  Both alias entries — the child-session entry and the delegating-parent
  entry the delegate path registers — resolve to the same Run.
  """
  target = task_manager().aliases.resolve_thread(owner_session_id, thread_id)
  if not target:
    return None
  session_id, run_id = target["session_id"], target["run_id"]
  run = await asyncio.to_thread(task_manager().runs.read_run_sync, session_id, run_id)
  return (session_id, run_id) if run is not None else None


# Reads from read_thread_worker_events, per events-log path. The workers-panel
# poll re-reads the same append-only log every 5 s per expanded running worker,
# so parsed results are retained and a call parses only the bytes appended since
# the last one. A file that shrank (truncate/rewrite) restarts its entry.
_THREAD_EVENTS_CACHE_CAP = 32


class _ThreadEventsCacheEntry:
  __slots__ = ("events", "offset", "tool_id_to_name")

  def __init__(self) -> None:
    self.events: list[WorkerEvent] = []
    self.offset = 0
    self.tool_id_to_name: dict[str, str] = {}


_thread_events_cache: BoundedMemo[str, _ThreadEventsCacheEntry] = BoundedMemo(_THREAD_EVENTS_CACHE_CAP)
# Serializes the entry's incremental read (stat, tail bytes, append) so two
# polls of one log cannot interleave offset bookkeeping; the cache map's own
# LRU and eviction live in BoundedMemo.
_thread_events_lock = threading.Lock()


def read_thread_worker_events(events_path: Path) -> list[WorkerEvent]:
  """Project a worker's events.jsonl into the events endpoint's full WorkerEvent list.

  The caller gets the complete projected history on every call; an unchanged
  log costs one stat, and between polls only newly appended complete lines are
  parsed. A trailing partial line (writer mid-append) is left for the next
  call, so nothing the writer has not committed reaches the projection.
  """
  key = str(events_path)
  with _thread_events_lock:
    if not events_path.exists():
      _thread_events_cache.drop(key)
      return []
    entry = _thread_events_cache.get(key)
    size = events_path.stat().st_size
    if entry is None or size < entry.offset:
      entry = _ThreadEventsCacheEntry()
    if size > entry.offset:
      with open(events_path, "rb") as f:
        f.seek(entry.offset)
        window = f.read(size - entry.offset)
      complete_end = window.rfind(b"\n") + 1
      if complete_end:
        raw_events = list(
            iter_ndjson_events(window[:complete_end].split(b"\n"), log_event=PARSE_SKIP_LOG_EVENT, log_fields={}))
        _append_worker_events(raw_events, entry.events, entry.tool_id_to_name)
        entry.offset += complete_end
    _thread_events_cache.store(key, entry)
    return list(entry.events)


def read_thread_worker_events_memo_hit(events_path: Path) -> list[WorkerEvent] | None:
  """Serve the unchanged-log steady state on the caller's thread; None otherwise.

  The 5 s workers-panel poll of an unchanged log needs one stat to
  prove the memo current, and the executor round-trip around it measures
  ~95 us against a ~12 us hit. Returns None — the caller re-runs
  ``read_thread_worker_events`` on a thread — for a cold memo, a grown or
  shrunk log, or a lock held by a concurrent reader's incremental read: the
  holder may be mid-file-read, so this path never waits on the lock.
  """
  key = str(events_path)
  if not _thread_events_lock.acquire(blocking=False):
    return None
  try:
    entry = _thread_events_cache.get(key)
    if entry is None:
      return None
    try:
      size = events_path.stat().st_size
    except OSError:
      return None
    if size != entry.offset:
      return None
    return list(entry.events)
  finally:
    _thread_events_lock.release()


def _append_worker_events(
    raw_events: Iterable[dict], events: list[WorkerEvent], tool_id_to_name: dict[str, str]) -> None:
  for data in raw_events:
    event_timestamp = data.get("timestamp") or datetime.now(UTC)
    event_type = data.get('type', '')
    # The run-start adoption signal is the log's session-id record (the token
    # tally's codex reconciliation reads it from the raw line), never a panel
    # row: the typed gate sits before the WorkerEvent construction a type-less
    # line cannot pass.
    if event_type == ET.SESSION_ATTACHED:
      continue
    if event_type == ET.ASSISTANT and isinstance(data.get('message'), dict):
      text = extract_text_from_message(data['message'])
      if text:
        events.append(WorkerEvent(type=ET.ASSISTANT, content=text, timestamp=event_timestamp))
      for block in data['message'].get('content', []):
        if isinstance(block, dict) and block.get('type') == 'tool_use':
          tool_id_to_name[block['id']] = block['name']
          events.append(
              WorkerEvent(
                  type=ET.TOOL_USE,
                  tool_name=block['name'],
                  # The panel renders the same one-line input summary the chat
                  # wire's renderer reads (toolInputSummary), so the row's
                  # input rides the same preview bound; the persisted events
                  # log keeps the full input.
                  input=tool_preview({
                      "name": block["name"],
                      "input": block.get('input', {})
                  })["input"],
                  timestamp=event_timestamp,
              ))
    elif event_type == ET.USER and isinstance(data.get('message'), dict):
      for block in data['message'].get('content', []):
        if block.get('type') == 'tool_result':
          tool_use_id = block.get('tool_use_id', '')
          name = tool_id_to_name.get(tool_use_id, '')
          result_text = extract_tool_result_text(block)
          # The projected row rides every full fetch, and the client renders an
          # output's first TOOL_PREVIEW_CHARS characters inline (the chat wire's
          # bound), so the row carries exactly that preview and its marker — the
          # persisted events log keeps the full text.
          truncated = len(result_text) > TOOL_PREVIEW_CHARS
          events.append(
              WorkerEvent(
                  type=ET.TOOL_RESULT,
                  tool_name=name,
                  content=result_text[:TOOL_PREVIEW_CHARS] if truncated else result_text,
                  output_truncated=True if truncated else None,
                  timestamp=event_timestamp,
              ))
    else:
      try:
        row = WorkerEvent(**{k: v for k, v in data.items() if k in WorkerEvent.model_fields})
      except Exception as e:
        log.debug('event_parse_failed', error=str(e))
        row = WorkerEvent(type='raw', content=str(data))
      # A top-level tool_result line carries its output in content; the same
      # TOOL_PREVIEW_CHARS wire bound as the message-nested branch above bounds
      # the projected row.
      if row.type == ET.TOOL_RESULT and row.content is not None and len(row.content) > TOOL_PREVIEW_CHARS:
        row.content = row.content[:TOOL_PREVIEW_CHARS]
        row.output_truncated = True
      events.append(row)


@router.get("/{session_id}/threads/{thread_id}/events", response_model=list[WorkerEvent])
async def get_thread_events(
    request: Request,
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    after: int | None = Query(default=None, ge=0),
) -> Response:
  """Return historical Worker events from the on-disk events.jsonl log.

  Without ``after`` the response is the full projected list. With ``after``
  (the client's rendered raw count) it is an envelope ``{"events", "total",
  "reset"}`` carrying only the events past that count; ``reset`` marks the
  count as ahead of the projection (log replaced), so the client re-renders
  the envelope's full payload. The projection is append-only
  (_append_worker_events never rewrites an emitted row), so a count inside
  it is a sound prefix cut.
  """
  v2_run = await _resolve_v2_run(session_id, thread_id)
  if v2_run is not None:
    # The worker Run's own events log: the same projection the v2 route
    # serves, reached through the registered alias.
    events_path = task_manager().runs.run_dir(v2_run[0], v2_run[1]) / "events.jsonl"
  else:
    events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  # The unchanged-log poll is one stat + a lookup; only a miss pays the
  # executor round-trip the incremental read needs.
  events = read_thread_worker_events_memo_hit(events_path)
  if events is None:
    events = await asyncio.to_thread(read_thread_worker_events, events_path)
  # Both shapes ride pre-dumped rows through FastJsonResponse: a Response skips
  # response_model validation, whose jsonable_encoder pass is ~6x model_dump on
  # mapped returns.
  if after is None:
    body = fast_json_bytes([e.model_dump(mode="json") for e in events])
    return await gzip_body_response(request, body, {}, _events_gzip_memo)
  reset = after > len(events)
  start = 0 if reset else after
  return FastJsonResponse(
      {
          "events": [e.model_dump(mode="json") for e in events[start:]],
          "total": len(events),
          "reset": reset
      })


@router.post("/{session_id}/threads/{thread_id}/cancel")
async def cancel_thread(
    session_id: str,
    thread_id: str,
    thread_mgr: ThreadManager = Depends(get_thread_manager),
    run_store=Depends(get_run_store),
    task_mgr=Depends(get_task_manager),
    caller: CallerIdentity = Depends(require_caller),
) -> dict:
  """Cancel a running thread (sends SIGTERM to the subprocess's process group).

  A v2 alias resolution (new-run compatibility alias, or an imported old id)
  routes to the Run owner's stop implementation instead: the same durable
  request, identity check, terminal fact and agent own-run scope as the v2
  cancel route — and no legacy ThreadMetadata status copy is ever written for
  it.
  """
  thread = await thread_mgr.get_thread(session_id, thread_id)
  if thread is None:
    alias = task_mgr.aliases.resolve_thread(session_id, thread_id)
    if alias is not None:
      target_session, target_run = alias["session_id"], alias["run_id"]
      if not caller.is_operator:
        # The v2 cancel route's own-run scope applies on the alias path too:
        # a run token never authorizes stopping another session's Run.
        claims = caller.claims
        assert claims is not None
        if claims.session_id != target_session or claims.run_id != target_run:
          raise HTTPException(status_code=403, detail="an agent may only stop its own bound run")
      try:
        result = await run_store.request_stop(target_session, target_run, f"thread-cancel:{thread_id}")
      except (RunNotFoundError, RunIdentityConflictError) as e:
        from src.api.sessions import _task_http_error
        raise _task_http_error(e) from e
      return {"run_id": result.run_id, "stop_requested": result.stop_requested, "outcome": result.outcome}
    raise HTTPException(status_code=404, detail=_THREAD_NOT_FOUND_DETAIL)

  if thread.pid:
    kill_process_group(thread.pid)

  await thread_mgr.update_status(session_id, thread_id, ThreadStatus.CANCELLED)
  return {"ok": True}
