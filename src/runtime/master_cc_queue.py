"""Per-session queue for task manager Runs using the master CC harness."""

import asyncio
import datetime
from collections.abc import Awaitable, Callable
from typing import Any

from src.infra import config, log_once, models
from src.infra import event_types as ET
from src.runtime import master_cc_run, master_cc_state, sidebar_state, streaming, thinking_state
from src.runtime.agent_process import base
from src.runtime.hooks import backend_types, turn_contributions

log = log_once.LazyStructlogLogger()


def _enqueue_work_item(session_id: str, work_item: master_cc_state._WorkItem) -> tuple[datetime.datetime, bool]:
  """Atomically mark busy, queue the item, and ensure a consumer exists.

  No await and no statement that can raise after the state's first mutation: a
  work item in the queue always implies busy_since is set; the consumer clears
  it only at teardown, in its own await-free sequence. No other statement may
  interleave with this block — correctness of the busy-state invariant depends
  on that. The declared input type is validated before any state changes, so a
  bad declaration fails the caller without touching the queue or busy state.
  """
  if session_id not in master_cc_state._session_queues:
    master_cc_state._session_queues[session_id] = asyncio.Queue()
  # A resume item is re-attaching a turn that already started, so its busy
  # interval begins at the recorded start rather than at this enqueue.
  resumed = work_item.resume_record
  thinking_since, created = thinking_state.mark_busy(session_id, since=resumed.started_at if resumed else None)
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
    thinking_since: datetime.datetime | None,
    auto_trigger: bool,
) -> None:
  """Notify the sidebar of a busy-state change.

  Single construction site for the RUNNING_CHANGED payload keys;
  web/static/js/websocket.js reads them off the event verbatim.
  """
  await streaming.streaming_manager.broadcast(
      streaming.SIDEBAR_CHANNEL,
      {
          "type": ET.RUNNING_CHANGED,
          "session_id": session_id,
          sidebar_state.HAS_RUNNING_TASKS: has_running_tasks,
          "thinking_since": thinking_since.isoformat() if thinking_since else None,
          "auto_trigger": auto_trigger,
      },
  )


async def _enqueue_and_notify(session_id: str, work_item: master_cc_state._WorkItem, *, auto_trigger: bool) -> None:
  """Queue one work item, then notify the sidebar when this call opened a new busy interval.

  The enqueue is atomic (see _enqueue_work_item); the broadcast is a pure
  notification — correctness comes from readers deriving the state.
  """
  thinking_since, created = _enqueue_work_item(session_id, work_item)
  if created:
    await _broadcast_running_changed(
        session_id,
        has_running_tasks=True,
        thinking_since=thinking_since,
        auto_trigger=auto_trigger,
    )


async def _persist_with_readback(
    callbacks: models.SessionCallbacks,
    persist: Callable[..., Awaitable[str | None]],
    session_id: str,
    value: str,
    source: str,
    subject: str,
    **persist_kwargs: Any,
) -> None:
  """Persist *value* through *persist* and verify the value read back from disk.

  *persist_kwargs* rides through to *persist* (the resume anchor's
  ``native_backend=<producing backend id>``). A mismatch logs and broadcasts an
  ERROR event tagged *source*, so a save that did not land reaches the
  operator's chat panel instead of passing silently.
  """
  read_back = await persist(session_id, value, **persist_kwargs)
  if read_back == value:
    return
  log.error(f"{source}_persist_mismatch", session=session_id, written=value, read_back=read_back)
  await callbacks.persist_and_broadcast(
      session_id, {
          "type": ET.ERROR,
          "source": source,
          "message": f"{subject} persist mismatch: wrote {value!r}, read back {read_back!r} from disk",
      })


async def _refresh_anchors_from_disk(
    item: master_cc_state._WorkItem,
    session_id: str,
    last_cc_session_id: str | None,
    last_account_label: str | None,
) -> None:
  """Overwrite the dequeued item's anchor snapshot with what disk holds.

  The queue item carries the session metadata as it stood at enqueue time; by
  the time it dequeues, disk is the authority. The previous round persisted
  the account holding the transcript and the backend that produced the id.
  The read is the bypass-cache fresh read
  (``read_metadata_fresh``: no cache populate, so no second cache). A snapshot
  anchor that is set is refreshed to the disk value, including a disk-cleared
  one; a snapshot anchor that is None is never resurrected from disk -- a
  snapshot dequeuing without an anchor declares this round starts without one
  (a fresh session, or the stale-resume retry's deliberately cleared copy).
  Empty fields fill only from the consumer's own just-finished round (the
  consumer loop's ``last_*`` locals): a follow-up enqueued mid-round carries a
  snapshot taken before that round's anchor persist lands, and the relay is
  what resumes the same conversation. On a failed disk read
  (raised or missing metadata) the relay alone applies.
  """
  from src.runtime.sessions import SessionManager
  fresh = await SessionManager(item.cfg).read_metadata_fresh(session_id)
  meta = item.session_meta
  if fresh is not None:
    if meta.cc_session_id is not None:
      meta.cc_session_id = fresh.cc_session_id
    if meta.native_backend is not None:
      meta.native_backend = fresh.native_backend
    keeper = backend_types.account_keeper(meta)
    if keeper is not None:
      backend_types.record_account_label(meta, keeper.account_label(fresh))
  if last_cc_session_id and not meta.cc_session_id:
    meta.cc_session_id = last_cc_session_id
  if last_account_label and backend_types.account_keeper(meta) is None:
    backend_types.record_account_label(meta, last_account_label)


async def _session_consumer(session_id: str) -> None:
  """Drain the per-session queue sequentially, one CC run at a time.

  Each queued item runs once, in arrival order.
  """
  queue = master_cc_state._session_queues[session_id]
  # Relay cc_session_id across items: queued _WorkItems may carry distinct
  # SessionMetadata instances (e.g. fork bootstrap vs. user message loaded later).
  last_cc_session_id: str | None = None
  # Same relay for the pool account holding that transcript.
  last_account_label: str | None = None
  # Teardown context is captured per item because `item` is unbound if the
  # consumer exits before its first queue.get() returns.
  teardown_callbacks: models.SessionCallbacks | None = None
  teardown_auto_trigger = False
  try:
    while True:
      head: master_cc_state._WorkItem = await queue.get()
      item = head
      master_cc_state._current_items[session_id] = item
      teardown_callbacks = item.callbacks
      teardown_auto_trigger = item.auto_trigger
      try:
        # Disk is the authority at dequeue time; the last_* relay below is the
        # failed-read fallback (see _refresh_anchors_from_disk).
        await _refresh_anchors_from_disk(item, session_id, last_cc_session_id, last_account_label)
        result = await (
            master_cc_run._resume_cc(item) if item.resume_record is not None else master_cc_run._run_cc(item))
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
            resume_ref = cc_session_id or "fresh session"
            zero_err = base.make_error_event(
                f"Master run produced zero model output (cc_session_id={resume_ref}): "
                f"the turn settled with an all-zero usage result and no assistant text, "
                f"thinking, tool use, or manual compaction. "
                f"The triggering message was left unread. "
                f"One known cause: opencode message-ID wraparound (LESSONS.md, 2026-08-14).")
            await item.callbacks.persist_and_broadcast(session_id, zero_err)
          exit_code = 1

        # A fresh-native task turn never adopts the previous turn's anchor:
        # its conversation was deliberately started fresh.
        if item.task_run.fresh_native_context:
          last_cc_session_id = None
        # The backend the round actually ran on (the option _run_cc resolved,
        # carried out in finish_extras), recorded beside the id it produced.
        ran_backend: str | None = finish_extras.get("native_backend")
        # Update session_meta.cc_session_id for subsequent queued runs.
        if cc_session_id:
          item.session_meta.cc_session_id = cc_session_id
          if ran_backend:
            item.session_meta.native_backend = ran_backend
          last_cc_session_id = cc_session_id
          # The consumer is the single owner of persisting the resume anchor:
          # every round, unconditionally, with no comparison against any
          # in-memory value. The read-back verifies the write landed on disk.
          # Only a truthy id persists: a turn that ended without a backend
          # session (a refusal or a spawn/transport failure) must not wipe the
          # durable anchor — a fresh-native clear is the adapter's spawn-time
          # write, not this path. The producing backend lands in the same
          # anchor write, so the continuation rule can judge the id's owner.
          await _persist_with_readback(
              item.callbacks,
              item.callbacks.persist_cc_session_id,
              session_id,
              cc_session_id,
              "resume_anchor",
              "Resume anchor",
              native_backend=ran_backend,
          )

        # The pool account holding the transcript is persisted the same way,
        # every round with a read-back: a relay or a pool-wide transcript search
        # that moved the anchor must survive the next whole-object save.
        keeper = backend_types.account_keeper(item.session_meta)
        account_label = keeper.account_label(item.session_meta) if keeper is not None else None
        if keeper is not None and account_label and item.callbacks.persist_account_label is not None:
          last_account_label = account_label
          await _persist_with_readback(
              item.callbacks,
              item.callbacks.persist_account_label,
              session_id,
              account_label,
              keeper.account_source,
              keeper.account_subject,
          )

        # Computed once, with no re-check: a queued item keeps this round's
        # busy interval alive, so the only question is whether one is queued.
        still_thinking = not queue.empty()

        # Broadcast MASTER_DONE. thinking_seconds is the length of the
        # continuous busy interval reported by thinking_state, attached only
        # when this round leaves the queue empty.
        thinking_seconds = None
        if not still_thinking:
          busy_start = thinking_state.busy_since(session_id)
          if busy_start is not None:
            thinking_seconds = int((datetime.datetime.now(datetime.UTC) - busy_start).total_seconds())

        done_event = base.make_master_done_event(exit_code, still_thinking=still_thinking)
        if item.user_event_ids:
          done_event[ET.INPUT_EVENT_IDS] = list(item.user_event_ids)
        if thinking_seconds is not None:
          done_event[ET.THINKING_SECONDS] = thinking_seconds
        done_event.update(finish_extras)
        await item.callbacks.persist_and_broadcast(session_id, done_event)

        # The Run is the execution record; its finish callback lands the
        # observation and terminal fact after MASTER_DONE.
        assert item.on_task_finish is not None
        await item.on_task_finish(cc_session_id, exit_code, finish_extras)

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
    thinking_state.clear_busy(session_id)
    if teardown_callbacks is not None:
      active = teardown_callbacks.task_tree_activity(session_id)[0] if teardown_callbacks.task_tree_activity else False
      await _broadcast_running_changed(
          session_id,
          has_running_tasks=active,
          thinking_since=None,
          auto_trigger=teardown_auto_trigger,
      )


async def run_message(
    cfg: config.CharlieBotConfig,
    session_meta: models.SessionMetadata,
    user_content: str,
    callbacks: models.SessionCallbacks,
    *,
    user_event_ids: list[str],
    task_instructions: str,
    task_run: master_cc_state.TaskRunBinding,
    on_task_spawn: Callable[[int, str | None], Awaitable[None]],
    on_task_finish: Callable[[str | None, int, dict], Awaitable[None]],
    auto_trigger: bool = False,
    backend_option: models.BackendOption | None = None,
    extra_claude_flags: list[str] | None = None,
    uploaded_files: list[dict] | None = None,
    is_voice: bool = False,
    extra_env: dict[str, str] | None = None,
) -> str | None:
  """Queue one task manager Run through the shared CC process harness."""
  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)

  for contribution in turn_contributions.turn_contributions():
    await contribution.before_turn(session_meta, cfg)

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
      future=future,
      task_run=task_run,
      user_event_ids=list(user_event_ids),
      uploaded_files=uploaded_files,
      task_instructions=task_instructions,
      on_task_spawn=on_task_spawn,
      on_task_finish=on_task_finish,
      extra_env=extra_env,
  )
  await _enqueue_and_notify(session_meta.id, work_item, auto_trigger=auto_trigger)
  return await future


async def enqueue_master_resume(
    cfg: config.CharlieBotConfig,
    session_meta: models.SessionMetadata,
    record: models.MasterRunRecord,
    callbacks: models.SessionCallbacks,
    *,
    is_alive: Callable[[], bool],
    task_run: master_cc_state.TaskRunBinding,
    on_task_spawn: Callable[[int, str | None], Awaitable[None]],
    on_task_finish: Callable[[str | None, int, dict], Awaitable[None]],
    extra_env: dict[str, str] | None = None,
) -> asyncio.Future:
  """Re-attach a recorded live master turn by queueing a resume-follow item.

  Goes through the normal per-session queue: the re-attached turn always
  drains before any queued or replayed turn spawns a new CLI against the same
  conversation. A v2 task-tree follow passes its Run's binding plus the
  adapter's spawn/finish hooks so the followed turn still records its outcome
  on the Run. Returns the future the consumer resolves with the followed
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
      future=future,
      user_event_ids=list(record.user_event_ids),
      resume_record=record,
      resume_is_alive=is_alive,
      task_run=task_run,
      on_task_spawn=on_task_spawn,
      on_task_finish=on_task_finish,
      extra_env=extra_env,
  )
  await _enqueue_and_notify(session_meta.id, work_item, auto_trigger=False)
  return future


def queued_user_event_ids(session_id: str) -> set[str]:
  """Input event ids the in-process queue currently owns for one session."""
  ids: set[str] = set()
  current = master_cc_state._current_items.get(session_id)
  if current is not None:
    ids.update(current.user_event_ids)
  queue = master_cc_state._session_queues.get(session_id)
  if queue is not None:
    for item in list(queue._queue):  # same-process snapshot; safe under the GIL
      ids.update(item.user_event_ids)
  return ids
