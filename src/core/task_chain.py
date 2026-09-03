"""Cron task step chains — the single owner of a ``steps`` task's worker chain.

A ``steps`` scheduled task fires its first worker on the cron tick; every later
step starts when the previous one exits 0, fed the previous step's result text
under a heading. The last step's completion (at any exit code) or any step's
failure wakes the session master exactly once with one summary block per step.
Chain position is derived from thread metadata (``chain_root``/``step_index``)
plus the task config on every call — the module stores nothing on disk beyond
those two thread fields.
"""

import structlog

from src.core import review
from src.core.config import CharlieBotConfig, ScheduledTaskConfig, get_scheduled_tasks
from src.core.models import SessionMetadata, ThreadMetadata
from src.core.sessions import SessionManager
from src.core.threads import ThreadManager

log = structlog.get_logger()


async def spawn_step(
    session: SessionMetadata,
    task_cfg: ScheduledTaskConfig,
    step_index: int,
    cfg: CharlieBotConfig,
    session_mgr: SessionManager,
    thread_mgr: ThreadManager,
    *,
    chain_root: str | None = None,
    previous_result: str | None = None,
) -> dict:
  """Spawn one step's worker and return the spawn dict plus the asyncio handle.

  The thread description is ``<task name> · <step name>`` with
  ``require_review=False``; ``chain_root`` defaults to the new thread's own id
  (the first step points at itself). The backend is the step's ``backend`` when
  set, else the task-level resolution; resolution, the spawn request, and the
  TASK_DELEGATED broadcast all run through ``fire_scheduled_worker``, the
  scheduler's single scheduled-worker spawn block. For ``step_index > 0`` the
  worker prompt appends the previous step's result under a heading. Returns
  ``{"session_id", "thread_id", "handle"}``.
  """
  # Lazy: src.core.scheduler imports this module at import scope, so importing
  # it back here would close the cycle.
  from src.core.scheduler import fire_scheduled_worker

  step = task_cfg.steps[step_index]
  description = f"{task_cfg.name} · {step.name}"
  thread = await thread_mgr.create_thread(session, description, require_review=False)
  thread.chain_root = chain_root if chain_root is not None else thread.id
  thread.step_index = step_index
  await thread_mgr.save_metadata(thread)

  prompt = step.prompt
  if step_index > 0:
    previous_name = task_cfg.steps[step_index - 1].name
    prompt = f"{prompt.rstrip()}\n\n## Result of the previous step ({previous_name})\n{previous_result}"

  handle = await fire_scheduled_worker(
      session,
      task_cfg,
      thread,
      description,
      cfg,
      session_mgr,
      thread_mgr,
      backend_override=step.backend,
      prompt_override=prompt)

  log.info(
      "scheduled_task_step_fired",
      task=task_cfg.name,
      step=step.name,
      step_index=step_index,
      session=session.id,
      thread=thread.id)

  return {"session_id": session.id, "thread_id": thread.id, "handle": handle}


def _chain_step_name(thread: ThreadMetadata, task: ScheduledTaskConfig | None) -> str:
  """The summary-block name for a chain thread: the config's step name, falling
  back to the thread description's ``<task> · <step>`` suffix when the task no
  longer declares (or no longer exists with) that step."""
  steps = task.steps if task is not None else None
  if steps is not None and thread.step_index is not None and 0 <= thread.step_index < len(steps):
    return steps[thread.step_index].name
  return thread.description.rsplit(" · ", 1)[-1]


async def _chain_summary_blocks(
    session_id: str,
    chain_root: str,
    task: ScheduledTaskConfig | None,
    thread_mgr: ThreadManager,
    cfg: CharlieBotConfig,
) -> list[str]:
  """One ``**<step name> result:**`` block per chain thread, ordered by step_index.

  An empty extracted result reads as a failure marker (an empty result is a
  failure, never a success).
  """
  threads = await thread_mgr.list_threads(session_id)
  chain = sorted(
      (t for t in threads if t.chain_root == chain_root and t.step_index is not None), key=lambda t: t.step_index)
  blocks: list[str] = []
  for t in chain:
    _, result = await review.extract_review_context(session_id, t.id, cfg.sessions_dir)
    blocks.append(f"**{_chain_step_name(t, task)} result:**\n{result or '(no result)'}")
  return blocks


async def _wake_master_with_chain(
    session_id: str,
    thread: ThreadMetadata,
    task: ScheduledTaskConfig | None,
    note: str | None,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
) -> None:
  """Wake the master once with the chain's per-step blocks plus an optional note.

  Reuses the reviewer path's effect-keyed idempotency (a master output after
  this thread's terminal summary means "woke"), so a completion-handler rerun
  never wakes twice.
  """
  blocks = await _chain_summary_blocks(session_id, thread.chain_root, task, thread_mgr, cfg)
  summary = "\n\n".join(blocks)
  if note:
    summary = f"{summary}\n\n{note}" if summary else note
  await review._trigger_master_judged(session_id, summary, thread.id, cfg, session_mgr)


async def handle_step_completion(
    session_id: str,
    thread: ThreadMetadata,
    exit_code: int,
    events_summary: str,
    full_summary: str,
    thread_mgr: ThreadManager,
    session_mgr: SessionManager,
    cfg: CharlieBotConfig,
) -> bool:
  """Own a chain thread's completion: advance the chain or wake the master.

  Returns False when the thread is not a chain thread (no ``step_index``), so
  every other thread keeps today's finalize path. When it returns True the
  caller must run nothing further for this completion.
  """
  if thread.step_index is None:
    return False

  session = await session_mgr.get_session(session_id)
  task_name = session.scheduled_task if session is not None else None
  task = next((t for t in get_scheduled_tasks() if t.name == task_name), None)
  if task is None or not task.steps:
    cause = (
        f"scheduled task '{task_name}' no longer exists"
        if task is None else f"scheduled task '{task_name}' no longer declares steps")
    log.info("chain_stopped_task_gone", session=session_id, thread=thread.id, cause=cause)
    await _wake_master_with_chain(session_id, thread, task, f"Chain stopped: {cause}.", thread_mgr, session_mgr, cfg)
    return True

  steps = task.steps
  is_last = thread.step_index >= len(steps) - 1
  if exit_code == 0 and not is_last:
    next_index = thread.step_index + 1
    session_threads = await thread_mgr.list_threads(session_id)
    if any(t.chain_root == thread.chain_root and t.step_index == next_index for t in session_threads):
      # Idempotency for finalize reruns: a chain that already advanced spawns
      # nothing more and wakes nothing more on this thread.
      log.info("chain_advance_skip_already_spawned", session=session_id, thread=thread.id, next=next_index)
      return True
    _, previous_result = await review.extract_review_context(session_id, thread.id, cfg.sessions_dir)
    if previous_result:
      await spawn_step(
          session,
          task,
          next_index,
          cfg,
          session_mgr,
          thread_mgr,
          chain_root=thread.chain_root,
          previous_result=previous_result)
      return True
    note = f"Chain stopped: step '{steps[thread.step_index].name}' produced no result."
    log.info("chain_stopped_empty_result", session=session_id, thread=thread.id)
    await _wake_master_with_chain(session_id, thread, task, note, thread_mgr, session_mgr, cfg)
    return True

  # The last step finished (any exit code) or a step failed: one wake, no
  # backend retry.
  await _wake_master_with_chain(session_id, thread, task, None, thread_mgr, session_mgr, cfg)
  return True
