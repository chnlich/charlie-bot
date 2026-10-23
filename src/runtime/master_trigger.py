"""Master-agent triggering subsystem — dispatch task-tree inputs into Runs."""

from src.infra import event_types as ET
from src.infra import log_once, models, tasks
from src.runtime import session_lifecycle, session_store, session_successor, task_execution

log = log_once.LazyStructlogLogger()


async def _wake_task_node(
    store: session_store.SessionStore,
    node: models.SessionMetadata,
    *,
    requested_id: str,
    summary: str,
    input_id: str | None,
    event_type: str,
    actor: str,
    from_session: str | None,
    from_session_name: str | None,
) -> None:
  """Dispatch a wake into the resolved task node's pending input queue."""
  dispatch = task_execution.task_manager().dispatch
  if node.id != requested_id and from_session is None:
    predecessor = await store.get_session(requested_id)
    assert predecessor is not None
    from_session = predecessor.id
    from_session_name = predecessor.name
  await dispatch.admit_input(
      node.id,
      event_type=event_type,
      content=summary,
      actor=actor,
      input_id=input_id,
      from_session=from_session,
      from_session_name=from_session_name,
  )
  await dispatch.dispatch_pending(node.id)


async def trigger_master(
    session_id: str,
    summary: str,
    store: session_store.SessionStore,
    successor: session_successor.SessionSuccessor,
    lifecycle: session_lifecycle.SessionLifecycle,
    *,
    input_id: str | None = None,
    event_type: str = ET.AGENT_MESSAGE,
    actor: str = "agent",
    from_session: str | None = None,
    from_session_name: str | None = None,
    pull_back: bool = True,
) -> None:
  """Dispatch a wake to the task node at the end of the session's succession chain."""
  resolved = await successor.resolve_successor_chain(session_id)
  if resolved is None:
    log.error("trigger_master_session_not_found", session=session_id)
    return

  if resolved.id != session_id:
    log.info("trigger_master_redirected_to_successor", session=session_id, resolved_session=resolved.id)

  if resolved.status == models.SessionStatus.ARCHIVED and resolved.successor_session_id is None:
    if not pull_back:
      log.info("wake_skipped_archived", session=session_id, resolved_session=resolved.id)
      return
    await lifecycle.unarchive_session(resolved.id)
    log.info("wake_pulled_back_archived_session", session=session_id, resolved_session=resolved.id)

  session_meta = await store.get_session(resolved.id)
  if session_meta is None:
    log.error("trigger_master_session_not_found", session=resolved.id)
    return

  await _wake_task_node(
      store,
      session_meta,
      requested_id=session_id,
      summary=summary,
      input_id=input_id,
      event_type=event_type,
      actor=actor,
      from_session=from_session,
      from_session_name=from_session_name,
  )


def spawn_master_turn(
    session_id: str,
    summary: str,
    store: session_store.SessionStore,
    successor: session_successor.SessionSuccessor,
    lifecycle: session_lifecycle.SessionLifecycle,
    *,
    input_id: str,
    event_type: str,
    actor: str,
    from_session: str,
    from_session_name: str,
    task_name: str,
) -> None:
  """Run one master turn as a logged background task named *task_name*.

  A wake never blocks its caller: the trigger coroutine runs on the logged-task
  spawner, whose done callback logs the trigger's exception instead of letting
  it die silently. ``trigger_master`` resolves through this module attribute at
  call time, so tests patch the wake there and the spawn still routes through
  the stand-in.
  """
  tasks.create_logged_task(
      trigger_master(
          session_id,
          summary,
          store,
          successor,
          lifecycle,
          input_id=input_id,
          event_type=event_type,
          actor=actor,
          from_session=from_session,
          from_session_name=from_session_name,
      ),
      name=task_name)
