"""Master run queueing — per-session consumer, run/cancel/resume entry points, restart replay."""

import asyncio
from collections.abc import Callable
from datetime import datetime, timezone
from typing import TYPE_CHECKING

import structlog

from src.agents import master_cc_run, master_cc_state
from src.agents.backends.base import make_error_event
from src.core import event_types as ET
from src.core import runs
from src.core.config import CharlieBotConfig
from src.core.latex import get_tex_path, snapshot_tex
from src.core.models import (
  BackendOption,
  MasterRunRecord,
  SessionCallbacks,
  SessionMetadata,
)
from src.core.process import kill_group_escalating
from src.core.streaming import streaming_manager
from src.core.thinking_state import busy_since, clear_busy, mark_busy

if TYPE_CHECKING:
  from src.core.sessions import SessionManager

log = structlog.get_logger()


def _enqueue_work_item(session_id: str, work_item: master_cc_state._WorkItem) -> tuple[datetime, bool]:
  """Atomically mark busy, queue the item, and ensure a consumer exists.

  No await and no statement that can raise: a work item in the queue always
  implies busy_since is set; the consumer clears it only at teardown, in its
  own await-free sequence. No other statement may interleave with this block —
  correctness of the busy-state invariant depends on that.
  """
  if session_id not in master_cc_state._session_queues:
    master_cc_state._session_queues[session_id] = asyncio.Queue()
  # A resume item is re-attaching a turn that already started, so its busy
  # interval begins at the recorded start rather than at this enqueue.
  resumed = work_item.resume_record
  thinking_since, created = mark_busy(session_id, since=resumed.started_at if resumed else None)
  master_cc_state._session_queues[session_id].put_nowait(work_item)
  if session_id not in master_cc_state._session_consumers or master_cc_state._session_consumers[session_id].done():
    master_cc_state._session_consumers[session_id] = asyncio.create_task(
        _session_consumer(session_id),
        name=f"master-consumer-{session_id[:8]}",
    )
  return thinking_since, created


async def _broadcast_running_changed(
    session_id: str,
    *,
    has_running_tasks: bool,
    thinking_since: datetime | None,
    auto_trigger: bool,
) -> None:
  """Notify the sidebar of a busy-state change.

  Single construction site for the RUNNING_CHANGED payload keys;
  web/static/js/websocket.js reads them off the event verbatim.
  """
  await streaming_manager.broadcast(
      "sidebar",
      {
          "type": ET.RUNNING_CHANGED,
          "session_id": session_id,
          "has_running_tasks": has_running_tasks,
          "thinking_since": thinking_since.isoformat() if thinking_since else None,
          "auto_trigger": auto_trigger,
      },
  )


async def _session_consumer(session_id: str) -> None:
  """Drain the per-session queue sequentially, one CC run at a time."""
  queue = master_cc_state._session_queues[session_id]
  # Relay cc_session_id across items: queued _WorkItems may carry distinct
  # SessionMetadata instances (e.g. fork bootstrap vs. user message loaded later).
  last_cc_session_id: str | None = None
  # Teardown context for the idle RUNNING_CHANGED broadcast, captured per item
  # so the finally never reads the loop variable — `item` is unbound when the
  # consumer exits (e.g. via cancellation) before the first queue.get() returns.
  teardown_cfg: CharlieBotConfig | None = None
  teardown_auto_trigger = False
  try:
    while True:
      item: master_cc_state._WorkItem = await queue.get()
      master_cc_state._current_items[session_id] = item
      teardown_cfg = item.cfg
      teardown_auto_trigger = item.auto_trigger
      try:
        # Carry the previous run's cc_session_id onto a freshly-loaded meta
        # so --resume picks up the in-progress CC transcript.
        if last_cc_session_id and not item.session_meta.cc_session_id:
          item.session_meta.cc_session_id = last_cc_session_id
        result = await (master_cc_run._resume_cc(item)
                        if item.resume_record is not None else master_cc_run._run_cc(item))
        cc_session_id, exit_code, _error_msg, finish_extras = result

        # Zero-output guard: a run that settled with a result event of all-zero
        # usage, no assistant text/thinking/tool_use, and no manual compaction
        # must fail loudly — the backend reported it as done but produced
        # nothing, so the triggering message would otherwise be consumed
        # silently. Lives on the single MASTER_DONE path, so both the
        # fresh-run and resume outcomes are covered by construction. If an
        # error event was already emitted this turn (error_msg set), skip a
        # second contradicting ERROR but still exit nonzero — mirrors the
        # salvage rule's error_msg gate. mark_unread is already invoked by
        # both run paths' teardown.
        if finish_extras.get("zero_output"):
          if not _error_msg:
            resume_ref = cc_session_id if cc_session_id else "fresh session"
            zero_err = make_error_event(
                f"Master run produced zero model output (cc_session_id={resume_ref}): "
                f"the turn settled with an all-zero usage result and no assistant text, "
                f"thinking, tool use, or manual compaction. "
                f"The triggering message was left unread. "
                f"One known cause: opencode message-ID wraparound (LESSONS.md, 2026-08-14).")
            await item.callbacks.persist_and_broadcast(session_id, zero_err)
          exit_code = 1

        # Update session_meta.cc_session_id for subsequent queued runs.
        if cc_session_id:
          item.session_meta.cc_session_id = cc_session_id
          last_cc_session_id = cc_session_id
          # The consumer is the single owner of persisting the resume anchor:
          # every round, unconditionally, with no comparison against any
          # in-memory value. The read-back verifies the write landed on disk.
          read_back = await item.callbacks.persist_cc_session_id(session_id, cc_session_id)
          if read_back != cc_session_id:
            log.error(
                "resume_anchor_persist_mismatch",
                session=session_id,
                written=cc_session_id,
                read_back=read_back,
            )
            await item.callbacks.persist_and_broadcast(session_id, {
                "type": ET.ERROR,
                "source": "resume_anchor",
                "message": (
                    f"Resume anchor persist mismatch: wrote {cc_session_id!r}, "
                    f"read back {read_back!r} from disk"),
            })

        # Computed once, with no re-check: a queued item keeps this round's
        # busy interval alive, so the only question is whether one is queued.
        still_thinking = not queue.empty()

        # Broadcast MASTER_DONE. thinking_seconds is the length of the
        # continuous busy interval reported by thinking_state, attached only
        # when this round leaves the queue empty.
        thinking_seconds = None
        if not still_thinking:
          busy_start = busy_since(session_id)
          if busy_start is not None:
            thinking_seconds = int((datetime.now(timezone.utc) - busy_start).total_seconds())

        done_event = {"type": ET.MASTER_DONE, "exit_code": exit_code, "still_thinking": still_thinking}
        if item.user_event_id:
          done_event["input_event_id"] = item.user_event_id
        if thinking_seconds is not None:
          done_event["thinking_seconds"] = thinking_seconds
        done_event.update(finish_extras)
        await item.callbacks.persist_and_broadcast(session_id, done_event)

        # The turn is fully resolved — clear its restart-identity so the next
        # startup reconcile neither re-attaches nor replays its user message.
        # Written after MASTER_DONE: crash between the two replays the message
        # with a marker (duplicate-tolerant) rather than silently dropping it.
        await item.callbacks.persist_master_run(session_id, None)

        # Resolve the caller's future
        if not item.future.done():
          item.future.set_result(cc_session_id)

      except Exception as exc:
        log.exception("session_consumer_item_error", session=session_id)
        if not item.future.done():
          item.future.set_exception(exc)

      finally:
        queue.task_done()
        master_cc_state._current_items.pop(session_id, None)

      # If queue is empty, exit the consumer loop — it will be re-created lazily.
      if queue.empty():
        break
  finally:
    # Await-free teardown: the loop's queue.empty() exit check, this
    # deregistration, and the busy-state clear form one synchronous sequence —
    # an enqueue cannot interleave inside it, so a new work item either extends
    # the current consumer (loop sees a non-empty queue) or starts a fresh
    # consumer that re-marks busy. No await is allowed in this sequence; that
    # property is what makes the busy-state invariant hang-free.
    master_cc_state._session_consumers.pop(session_id, None)
    # Clean up the queue if empty to avoid memory leaks from abandoned sessions.
    if session_id in master_cc_state._session_queues and master_cc_state._session_queues[session_id].empty():
      master_cc_state._session_queues.pop(session_id, None)
    clear_busy(session_id)
    if teardown_cfg is not None:
      # Check if workers are still running before declaring idle.
      from src.core.sessions import SessionManager
      workers_running = await SessionManager(teardown_cfg)._has_running_tasks(session_id)
      await _broadcast_running_changed(
          session_id,
          has_running_tasks=workers_running,
          thinking_since=None,
          auto_trigger=teardown_auto_trigger,
      )


async def run_message(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_content: str,
    callbacks: SessionCallbacks,
    skip_user_event: bool = False,
    auto_trigger: bool = False,
    backend_option: BackendOption | None = None,
    extra_claude_flags: list[str] | None = None,
    display_content: str | None = None,
    uploaded_files: list[dict] | None = None,
    is_voice: bool = False,
    expect_fresh_session: bool = False,
    user_event_id: str | None = None,
) -> str | None:
  """Spawn a Claude Code process for the master agent and stream NDJSON events.

  Args:
    cfg: App configuration.
    session_meta: The session to run in.
    user_content: The user's message text.
    callbacks: Bundle of session hooks (persist_and_broadcast,
      update_thinking_state, mark_unread, persist_cc_session_id,
      has_completed_round).
    skip_user_event: If True, skip persisting/broadcasting the user event
      (used when the master is triggered by a worker completion, not a real user message).
    display_content: User-visible content persisted to the chat log. Defaults
      to ``user_content`` when omitted.
    uploaded_files: Structured uploaded-file metadata persisted on the user event.
    expect_fresh_session: True only on the scheduled-session weekly-recycle
      path that deliberately clears the anchor; suppresses the
      resume-anchor-missing pre-flight alarm.
    user_event_id: Chat event id of the user message this turn answers;
      recorded in master_run so restart reconcile excludes exactly it from
      replay. Only pass explicitly on the replay path (skip_user_event=True);
      otherwise captured from the freshly persisted user event.

  Returns:
    The CC session ID (for --resume on subsequent messages), or None.
  """
  # tui-cli sessions are interactive terminal sessions: messages flow through
  # tmux, not the SDK. Skip the master agent entirely so we never spawn a
  # claude SDK subprocess for them.
  backend_id = session_meta.backend or (cfg.backend_options[0].id if cfg.backend_options else "")
  backend_lookup = cfg.get_backend_option(backend_id)
  if backend_lookup is not None and backend_lookup.type == "tui-cli":
    log.info("master_cc_skip_tui_backend", session=session_meta.id, backend=backend_id)
    return None

  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)

  tex_path = get_tex_path()
  should_check_tex = tex_path.exists()
  if should_check_tex:
    await asyncio.to_thread(snapshot_tex)

  # Persist the user message so it survives page refresh (WebSocket catch-up).
  # All awaits in run_message happen here, BEFORE the atomic enqueue block
  # below — between mark_busy and put_nowait nothing may yield or raise.
  if not skip_user_event:
    user_event = {
        "type": ET.USER,
        "content": user_content if display_content is None else display_content,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "is_voice": is_voice,
    }
    if uploaded_files:
      user_event["uploaded_files"] = uploaded_files
    await callbacks.persist_and_broadcast(session_meta.id, user_event)
    user_event_id = user_event.get("id")
    session_meta.updated_at = datetime.now(timezone.utc)
    await callbacks.update_thinking_state(session_meta.id, updated_at=session_meta.updated_at)

  # Create a future for the caller to await.
  loop = asyncio.get_running_loop()
  future: asyncio.Future = loop.create_future()

  work_item = master_cc_state._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content=user_content,
      callbacks=callbacks,
      is_voice=is_voice,
      auto_trigger=auto_trigger,
      backend_option=backend_option,
      extra_claude_flags=extra_claude_flags,
      should_check_tex=should_check_tex,
      future=future,
      expect_fresh_session=expect_fresh_session,
      user_event_id=user_event_id,
  )

  # --- atomic enqueue block: no await, no statement that can raise ---
  # Busy state is marked before the item enters the queue, so a work item in
  # the queue always implies busy_since is set; the consumer clears it only at
  # teardown, in its own await-free sequence.
  thinking_since, created = _enqueue_work_item(session_meta.id, work_item)
  # --- end atomic block ---

  # Notify only when this call opened a new busy interval; the broadcast is a
  # pure notification — correctness comes from readers deriving the state.
  if created:
    await _broadcast_running_changed(
        session_meta.id,
        has_running_tasks=True,
        thinking_since=thinking_since,
        auto_trigger=auto_trigger,
    )

  # Await until this specific work item completes.
  return await future


async def cancel_master(
    session_id: str,
    *,
    meta: SessionMetadata | None = None,
    session_mgr: "SessionManager | None" = None,
) -> bool:
  """Terminate the running master CC turn for this session.

  In-process hit: terminate the live backend. In-process miss with session
  metadata: the turn may have detached across a graceful restart, so fall
  back to the on-disk master_run record — an irreversible kill goes out only
  when ``runs.is_run_alive`` proves the recorded (pid, pid_start, started_at)
  triple still names a live process; an unprovable record gets no signal at
  all and the endpoint keeps its 404. Callers that pass neither optional keep
  the in-memory-only behavior.

  Returns True if a turn was found and signalled, False otherwise.
  """
  log.info("master_cancel_requested", session=session_id)
  backend = master_cc_state._active_procs.get(session_id)
  if backend is not None:
    await backend.terminate()
    log.info("master_cancel_succeeded", session=session_id)
    return True

  record = meta.master_run if meta is not None else None
  if record is not None and session_mgr is not None:
    host_boot = await asyncio.to_thread(runs.read_host_boot_time)

    def _alive() -> bool:
      return runs.is_run_alive(record.pid, record.pid_start, record.started_at, host_boot)

    if _alive() and record.pid is not None:
      # Detached turn still running: the record's own liveness proof authorized
      # this kill.
      log.info("master_cancel_killing_detached_run", session=session_id, pid=record.pid)
      await kill_group_escalating(record.pid, _alive)
      await session_mgr.persist_master_run(session_id, None)
      log.info("master_cancel_succeeded", session=session_id)
      return True

  log.info("master_cancel_no_active_master", session=session_id)
  return False


async def enqueue_master_resume(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    record: MasterRunRecord,
    callbacks: SessionCallbacks,
    *,
    is_alive: Callable[[], bool],
) -> asyncio.Future:
  """Re-attach a recorded live master turn by queueing a resume-follow item.

  Goes through the normal per-session queue: the re-attached turn always
  drains before any queued or replayed turn spawns a new CLI against the same
  conversation. Returns the future the consumer resolves with the followed
  turn's cc_session_id when its MASTER_DONE lands.
  """
  loop = asyncio.get_running_loop()
  future: asyncio.Future = loop.create_future()
  work_item = master_cc_state._WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content="",
      callbacks=callbacks,
      is_voice=False,
      auto_trigger=False,
      backend_option=None,
      extra_claude_flags=None,
      should_check_tex=False,
      future=future,
      user_event_id=record.user_event_id,
      resume_record=record,
      resume_is_alive=is_alive,
  )
  # Atomic (see _enqueue_work_item); the broadcast below is a pure
  # notification — correctness comes from readers deriving the state.
  thinking_since, created = _enqueue_work_item(session_meta.id, work_item)
  if created:
    await _broadcast_running_changed(
        session_meta.id,
        has_running_tasks=True,
        thinking_since=thinking_since,
        auto_trigger=False,
    )
  return future


def queued_user_event_ids(session_id: str) -> set[str]:
  """Chat event ids of user messages this process already owns (running or queued).

  Startup reconcile excludes exactly these (plus any recorded turn's id) from
  replay: within one process the queue survives, so replaying a queued message
  would answer it twice. The exclusion is per-event, never per-session — a
  message queued BEHIND a crashed turn disappears with the killed process and
  must be replayed.
  """
  ids: set[str] = set()
  current = master_cc_state._current_items.get(session_id)
  if current is not None and current.user_event_id:
    ids.add(current.user_event_id)
  queue = master_cc_state._session_queues.get(session_id)
  if queue is not None:
    for item in list(queue._queue):  # same-process snapshot; safe under the GIL
      if item.user_event_id:
        ids.add(item.user_event_id)
  return ids


_REPLAY_MARKER = (
    "[System: the server restarted while answering this message, so it is "
    "being redelivered. Your previous, interrupted attempt may already have "
    "performed some actions — before repeating any side effect (files "
    "written, messages sent, tasks delegated), read back that action's state "
    "first and continue from there instead.]")


async def replay_user_message(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_event: dict,
    callbacks: SessionCallbacks,
) -> None:
  """Redeliver an unanswered user message after a restart, marked as a replay.

  The original user event stays put in the chat log (skip_user_event); the
  replayed prompt prefixes ``_REPLAY_MARKER`` so the master checks prior side
  effects before redoing them. The record's user_event_id keeps pointing at
  the ORIGINAL event, which is the one startup reconcile must exclude.
  """
  content = user_event.get("content")
  if not isinstance(content, str) or not content:
    log.error("master_replay_unusable_event", session=session_meta.id, event_id=user_event.get("id"))
    return
  await run_message(
      cfg,
      session_meta,
      user_content=f"{_REPLAY_MARKER}\n\n{content}",
      callbacks=callbacks,
      skip_user_event=True,
      is_voice=bool(user_event.get("is_voice")),
      user_event_id=user_event.get("id"),
  )
