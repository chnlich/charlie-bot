"""Master-agent triggering subsystem — wake the master CC to process results.

The scheduled-task duties the old dedicated cron session carried on its wake
(the weekly recycle and the firing report's prefix) live here as the shared
helpers the bound node's dispatched wake consumes; ``trigger_master`` itself no
longer has a scheduled branch — the bound node took those duties over.
"""

import traceback
from typing import Any

from src.infra import event_types as ET
from src.infra.config import CharlieBotConfig
from src.infra.deferred import deferred_import_loader, deferred_module_getattr
from src.infra.log_once import LazyStructlogLogger
from src.infra.models import SessionMetadata, SessionStatus
from src.runtime.sessions import SessionManager

log = LazyStructlogLogger()

_load_run_message = deferred_import_loader("run_message", "src.runtime.master_cc_queue")


def __getattr__(name: str) -> Any:
  # The "src.runtime.master_trigger.run_message" patch target resolves through this hook.
  return deferred_module_getattr(name, __name__, globals(), "run_message", _load_run_message)


async def apply_sequence_wake_duties(
    session_mgr: SessionManager,
    meta: SessionMetadata,
    input_events: list[dict],
) -> str | None:
  """Run the registered sequence binding's wake duties, if the node has one."""
  from src.runtime.hooks.sequence_controllers import binding_for

  binding = binding_for(meta.id)
  if binding is None:
    return None
  return await binding.on_wake(meta, input_events, sessions=session_mgr)


async def run_message_with_resume_recovery(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    summary: str,
    session_mgr: SessionManager,
    expect_fresh_session: bool,
    user_event_id: str | None,
    input_event_type: str,
) -> str | None:
  """Call run_message, retrying once with cc_session_id cleared on stale-resume errors.

  ``expect_fresh_session``, ``user_event_id``, and ``input_event_type`` are
  forwarded to the first ``run_message`` call only. The stale-resume retry
  deliberately clears the anchor but must NOT forward the first two: an
  anchor-missing alarm there is correct (the resume failed and context is
  being dropped as recovery). The input type is the same wake either way, so
  the retry forwards it -- a stale-resume retry still answers the same input
  and must batch with whatever else is queued.
  """
  backend_id = session_meta.backend
  backend_option = cfg.get_backend_option(backend_id)
  run_message = _load_run_message(globals())
  try:
    return await run_message(
        cfg,
        session_meta,
        summary,
        session_mgr.callbacks(),
        input_event_type,
        skip_user_event=True,
        auto_trigger=True,
        backend_option=backend_option,
        expect_fresh_session=expect_fresh_session,
        user_event_id=user_event_id,
    )
  except Exception as e:
    if not is_resume_not_found_error(e):
      raise

    stale_cc_session_id = session_meta.cc_session_id
    log.warning(
        "trigger_master_invalid_resume_detected",
        session=session_meta.id,
        cc_session_id=stale_cc_session_id,
        error=str(e),
    )

    retry_session_meta = session_meta.model_copy(deep=True)
    retry_session_meta.cc_session_id = None
    log.info(
        "trigger_master_retry_without_resume",
        session=session_meta.id,
        stale_cc_session_id=stale_cc_session_id,
    )
    # expect_fresh_session intentionally not forwarded: an alarm on the retry
    # path is correct (context is being dropped as stale-resume recovery).
    new_cc_session_id = await run_message(
        cfg,
        retry_session_meta,
        summary,
        session_mgr.callbacks(),
        input_event_type,
        skip_user_event=True,
        auto_trigger=True,
        backend_option=backend_option,
    )
    log.info(
        "trigger_master_resume_recovery_succeeded",
        session=session_meta.id,
        stale_cc_session_id=stale_cc_session_id,
        recovered_cc_session_id=new_cc_session_id,
    )
    return new_cc_session_id


async def _wake_task_node(
    session_mgr: SessionManager, node: SessionMetadata, *, requested_id: str, summary: str) -> None:
  """Wake a task-tree node: dispatch its pending durable inputs into a Run.

  Every legacy wake persists its input into the requested session's log before
  it calls ``trigger_master``, and a task node reads its inputs from its own
  log. A wake that stayed on the requested node therefore finds its input
  pending already. A wake the succession chain redirected to another node left
  its input in the predecessor's log, so the successor receives it as an agent
  message from the predecessor.
  """
  # Lazy: the API layer owns the singleton, and this module stays on the light import floor.
  from src.runtime.api.deps import task_manager
  from src.runtime.control_events import ACTOR_SYSTEM

  dispatch = task_manager().dispatch
  if node.id != requested_id:
    predecessor = await session_mgr.get_session(requested_id)
    assert predecessor is not None  # the chain was resolved from it moments ago
    await dispatch.admit_input(
        node.id,
        event_type=ET.AGENT_MESSAGE,
        content=summary,
        actor=ACTOR_SYSTEM,
        from_session=predecessor.id,
        from_session_name=predecessor.name,
    )
  await dispatch.dispatch_pending(node.id)


async def trigger_master(
    session_id: str,
    summary: str,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    input_event_type: str,
    user_event_id: str | None = None,
    # Default True = pull back, so any wake path added later carries content and
    # unarchives its target without being listed here. Only the two timed wakes
    # (the trigger fire in src/runtime/triggers.py and the cron scheduler in
    # src/features/cron/scheduler.py) pass pull_back=False and keep the skip.
    pull_back: bool = True,
) -> None:
  """Best-effort trigger of the master agent to process a worker result.

  ``input_event_type`` is required with no default: every wake path declares
  which INPUT_EVENT_TYPES member (src/runtime/session_dispatch.py) its input is
  -- user messages never come through here, trigger wakes declare
  scheduled_trigger, cross-session messages and Slack injections declare
  agent_message, and worker/parent/improve reports declare child_report. The
  declared type rides the work item and names the input in a merged batch.

  A target archived without a successor is pulled back to active first when
  ``pull_back`` is set (the default); timed wakes opt out and skip instead.
  """
  target_session_id = session_id
  try:
    # Resolve through the succession chain: an elone may have landed since this
    # wake was scheduled, so the run targets the chain end rather than the
    # originally-requested session.
    resolved = await session_mgr.resolve_successor_chain(session_id)
    if resolved is None:
      log.error("trigger_master_session_not_found", session=session_id)
      return

    # The resolved target is what the wake falls back to for the error write.
    target_session_id = resolved.id

    if resolved.id != session_id:
      log.info(
          "trigger_master_redirected_to_successor",
          session=session_id,
          resolved_session=resolved.id,
      )

    # An archived session with no successor is the user's explicit "no more
    # wakes" signal. Timed wakes (pull_back=False) skip entirely: no run, no
    # event. Content wakes pull the session back to active first and continue.
    # An archived session WITH a successor has already been eloned and
    # redirects above instead.
    if resolved.status == SessionStatus.ARCHIVED and resolved.successor_session_id is None:
      if not pull_back:
        log.info(
            "wake_skipped_archived",
            session=session_id,
            resolved_session=resolved.id,
        )
        return
      await session_mgr.unarchive_session(resolved.id)
      log.info(
          "wake_pulled_back_archived_session",
          session=session_id,
          resolved_session=resolved.id,
      )

    session_meta = await session_mgr.get_session(resolved.id)
    if not session_meta:
      log.error("trigger_master_session_not_found", session=resolved.id)
      return

    # A v2 task-tree node is never woken through the legacy writer: its inputs
    # are durable dispatcher admissions and its turns are Runs.
    if session_meta.profile is not None:
      await _wake_task_node(session_mgr, session_meta, requested_id=session_id, summary=summary)
      return

    # Sequence duties run on the bound node's dispatched wake; this legacy
    # writer has no sequence branch.
    await run_message_with_resume_recovery(
        cfg,
        session_meta,
        summary,
        session_mgr,
        expect_fresh_session=False,
        user_event_id=user_event_id,
        input_event_type=input_event_type)
  except Exception as e:
    log.error("trigger_master_failed", session=session_id, error=str(e), traceback=traceback.format_exc())
    try:
      error_payload = {
          'type': ET.ERROR,
          'message': f'Failed to notify master agent: {e}',
          'source': 'trigger_master',
      }
      await session_mgr.persist_and_broadcast(target_session_id, error_payload)
    except Exception as persist_error:
      log.warning("trigger_master_error_event_persist_failed", session=target_session_id, error=str(persist_error))


def is_resume_not_found_error(error: Exception) -> bool:
  """Return True only for stale resume errors where session/conversation is missing."""
  message = str(error).lower()
  has_no_rollout_found = "no rollout found" in message and ("thread" in message or "resume failed" in message)
  if has_no_rollout_found:
    return True
  if "resume" not in message:
    return False

  has_conversation_not_found = "conversation" in message and "not found" in message
  has_session_not_found = "session" in message and "not found" in message
  return has_conversation_not_found or has_session_not_found
