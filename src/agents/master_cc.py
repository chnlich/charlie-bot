"""Master CC — spawns a Claude Code subprocess for the master agent."""

import asyncio
import dataclasses
import os
import time
from datetime import datetime, timezone
from typing import Optional

import structlog

from src.agents.backends.base import AgentBackend
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.latex import check_tex_changed, clear_snapshot, get_tex_path, snapshot_tex
from src.core.models import BackendOption, SessionMetadata
from src.core.streaming import handle_compact_boundary, streaming_manager

log = structlog.get_logger()

# Per-session FIFO queue for serializing run_message calls.
_session_queues: dict[str, asyncio.Queue] = {}

# One consumer task per session — drains the queue sequentially.
_session_consumers: dict[str, asyncio.Task] = {}

# Per-session running backend reference for external cancellation.
_active_procs: dict[str, AgentBackend] = {}


@dataclasses.dataclass
class _WorkItem:
  """All arguments needed to execute a single CC run, plus a future for the result."""
  cfg: CharlieBotConfig
  session_meta: SessionMetadata
  user_content: str
  persist_and_broadcast: object  # async callable
  save_metadata: object  # async callable or None
  mark_unread: object  # async callable or None
  clear_thinking_since: object  # async callable or None
  is_voice: bool
  auto_trigger: bool
  backend_option: Optional[BackendOption]
  extra_claude_flags: Optional[list[str]]
  should_check_tex: bool
  future: asyncio.Future


def _build_instructions_content(session_meta: SessionMetadata, cfg: CharlieBotConfig) -> Optional[str]:
  """Build master agent instructions by concatenating prompt + memory files."""
  prompt_file = cfg.claude_md_file
  if not prompt_file.exists():
    log.warning("master_prompt_file_missing", path=str(prompt_file))
    return None

  prompt_text = prompt_file.read_text(encoding="utf-8")
  prompt_text = prompt_text.replace("YOUR_SESSION_UUID", session_meta.id)
  parts = [prompt_text]
  for mf in [cfg.memory_file, cfg.memory_host_file]:
    if mf.exists():
      parts.append(mf.read_text(encoding="utf-8"))

  if session_meta.rewind_summary:
    parts.append(
        f"""# Session Rewind Context

This session was rewound from a previous conversation. Here is the conversation summary up to the rewind point:

{session_meta.rewind_summary}

Continue from this context. The user wants to take a different direction from this point.""")

  return "\n\n".join(parts)


def _build_prompt(user_content: str, is_voice: bool) -> str:
  """Prepend voice-transcription disclaimer when the message comes from voice input."""
  if is_voice:
    return (
        "[The following message is from voice transcription and might not be accurate. "
        "Please ask the user first for any words that are unclear or might be wrong.]\n"
        f"{user_content}")
  return user_content


async def _handle_event(
    event: dict,
    session_id: str,
    cc_session_id: Optional[str],
    persist_and_broadcast,
) -> Optional[str]:
  """Process a single backend event: persist, broadcast, and handle compact_boundary.

  Returns the cc_session_id (possibly updated from the event).
  """
  if not cc_session_id:
    sid = event.get("session_id")
    if sid:
      cc_session_id = sid

  # Persist first (injects timestamp), then broadcast with timestamp included
  await persist_and_broadcast(session_id, event)

  await handle_compact_boundary(
      event,
      persist_and_broadcast=lambda evt: persist_and_broadcast(session_id, evt),
      log_context={"session": session_id},
  )

  return cc_session_id


async def _run_cc(item: _WorkItem) -> tuple[Optional[str], int, Optional[str], dict]:
  """Execute a single CC run — spawn backend, stream events.

  Manages _active_procs for cancel support.  Does NOT broadcast MASTER_DONE
  or manage thinking state (the consumer loop handles that).

  Returns (cc_session_id, exit_code, error_msg, finish_extras).
  """
  cfg = item.cfg
  session_meta = item.session_meta
  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)
  cwd = str(session_dir)

  instructions_content = await asyncio.to_thread(_build_instructions_content, session_meta, cfg)

  from src.agents.backends.registry import build_backend
  option = item.backend_option or cfg.backend_options[0]

  extra_flags: list[str] = []
  resume_session = bool(session_meta.cc_session_id)
  if session_meta.cc_session_id and option.type not in ("codex", "gemini", "opencode"):
    extra_flags = ["--resume", session_meta.cc_session_id]
  if item.extra_claude_flags:
    extra_flags.extend(item.extra_claude_flags)

  env = {**os.environ}
  env.pop("CLAUDECODE", None)
  env["GIT_CEILING_DIRECTORIES"] = str(cfg.charliebot_home)
  env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"

  prompt = _build_prompt(item.user_content, item.is_voice)

  log.info(
      "master_cc_starting",
      session=session_meta.id,
      backend=option.type,
      model=option.model,
      prompt_chars=len(prompt),
      resume_session=resume_session,
      cwd=cwd,
  )

  cc_session_id: Optional[str] = session_meta.cc_session_id
  exit_code = 1
  error_msg: Optional[str] = None

  # Monotonic timing
  t_start = time.monotonic()
  t_spawn: Optional[float] = None
  t_first_event: Optional[float] = None
  t_first_assistant: Optional[float] = None

  async def _on_spawn(pid: int) -> None:
    nonlocal t_spawn
    t_spawn = time.monotonic()
    log.info("master_cc_spawned", session=session_meta.id, pid=pid, backend=option.type, model=option.model)

  backend: Optional[AgentBackend] = None
  saw_first_assistant = False

  try:
    backend = build_backend(
        option,
        cfg,
        extra_flags=extra_flags or None,
        buffer_limit=cfg.subprocess_buffer_limit,
        on_spawn=_on_spawn,
        instructions_content=instructions_content,
        resume_session_id=session_meta.cc_session_id if option.type in ("codex", "gemini", "opencode") else None,
    )
    _active_procs[session_meta.id] = backend

    async for event in backend.run(prompt, cwd, env):
      # Track first backend event
      if t_first_event is None:
        t_first_event = time.monotonic()
        spawn_ref = t_spawn if t_spawn is not None else t_start
        spawn_to_first_ms = int((t_first_event - spawn_ref) * 1000)
        log.info(
            "master_cc_first_event",
            session=session_meta.id,
            event_type=event.get("type"),
            spawn_to_first_event_ms=spawn_to_first_ms,
        )
        if spawn_to_first_ms > 10_000:
          log.warning(
              "master_cc_slow_first_event",
              session=session_meta.id,
              spawn_to_first_event_ms=spawn_to_first_ms,
          )

      # Track first assistant text
      if not saw_first_assistant and event.get("type") == ET.ASSISTANT:
        msg = event.get("message", {})
        content_blocks = msg.get("content") if isinstance(msg, dict) else None
        if content_blocks:
          for block in content_blocks:
            if isinstance(block, dict) and block.get("type") == "text" and block.get("text"):
              saw_first_assistant = True
              t_first_assistant = time.monotonic()
              first_assistant_ms = int((t_first_assistant - t_start) * 1000)
              log.info(
                  "master_cc_first_assistant_text",
                  session=session_meta.id,
                  first_assistant_ms=first_assistant_ms,
              )
              break

      cc_session_id = await _handle_event(event, session_meta.id, cc_session_id, item.persist_and_broadcast)

    exit_code = backend.exit_code
    if backend.stderr_text:
      log.warning("master_cc_stderr", session=session_meta.id, stderr=backend.stderr_text)
      if exit_code != 0:
        error_msg = backend.stderr_text[:500]

  except Exception as e:
    log.exception("master_cc_crashed", session=session_meta.id)
    error_msg = str(e)

  finally:
    _active_procs.pop(session_meta.id, None)
    total_ms = int((time.monotonic() - t_start) * 1000)

    # Build finish extras
    finish_extras: dict = {
        "backend": option.type,
        "model": option.model,
        "total_ms": total_ms,
    }
    if t_first_event is not None:
      spawn_ref = t_spawn if t_spawn is not None else t_start
      finish_extras["spawn_to_first_event_ms"] = int((t_first_event - spawn_ref) * 1000)
    if t_first_assistant is not None:
      finish_extras["first_assistant_ms"] = int((t_first_assistant - t_start) * 1000)

    if total_ms > 120_000:
      log.warning("master_cc_slow_total", session=session_meta.id, total_ms=total_ms)

    if error_msg:
      err_event = {"type": ET.ASSISTANT_ERROR, "content": f"Agent error: {error_msg}"}
      await item.persist_and_broadcast(session_meta.id, err_event)

    if item.mark_unread:
      await item.mark_unread(session_meta.id)

    if item.should_check_tex:
      proposal = await asyncio.to_thread(check_tex_changed)
      if proposal:
        tex_event = {'type': ET.TEX_EDIT_PROPOSED}
        await item.persist_and_broadcast(session_meta.id, tex_event)
        log.info('tex_edit_proposed', session=session_meta.id)
      else:
        clear_snapshot()

    log.info(
        "master_cc_finished",
        session=session_meta.id,
        exit_code=exit_code,
        **(finish_extras or {}),
    )

  return cc_session_id, exit_code, error_msg, finish_extras


async def _session_consumer(session_id: str) -> None:
  """Drain the per-session queue sequentially, one CC run at a time."""
  queue = _session_queues[session_id]
  try:
    while True:
      item: _WorkItem = await queue.get()
      try:
        # Update cc_session_id from session_meta before each run so --resume
        # uses the latest value (possibly set by a previous run in this batch).
        result = await _run_cc(item)
        cc_session_id, exit_code, _error_msg, finish_extras = result

        # Update session_meta.cc_session_id for subsequent queued runs.
        if cc_session_id:
          item.session_meta.cc_session_id = cc_session_id

        still_thinking = not queue.empty()

        # Broadcast MASTER_DONE
        thinking_seconds = None
        if not still_thinking and item.session_meta.thinking_since:
          thinking_seconds = int((datetime.now(timezone.utc) - item.session_meta.thinking_since).total_seconds())

        done_event = {"type": ET.MASTER_DONE, "exit_code": exit_code, "still_thinking": still_thinking}
        if thinking_seconds is not None:
          done_event["thinking_seconds"] = thinking_seconds
        done_event.update({k: v for k, v in finish_extras.items()})
        await item.persist_and_broadcast(session_id, done_event)

        if not still_thinking:
          item.session_meta.thinking_since = None
          if item.clear_thinking_since:
            await item.clear_thinking_since(session_id)
          await streaming_manager.broadcast(
              "sidebar", {
                  "type": ET.RUNNING_CHANGED,
                  "session_id": session_id,
                  "has_running_tasks": False,
                  "auto_trigger": item.auto_trigger,
              })

        # Resolve the caller's future
        if not item.future.done():
          item.future.set_result(cc_session_id)

      except Exception as exc:
        log.exception("session_consumer_item_error", session=session_id)
        if not item.future.done():
          item.future.set_exception(exc)

      finally:
        queue.task_done()

      # If queue is empty, exit the consumer loop — it will be re-created lazily.
      if queue.empty():
        break
  finally:
    _session_consumers.pop(session_id, None)
    # Clean up the queue if empty to avoid memory leaks from abandoned sessions.
    if session_id in _session_queues and _session_queues[session_id].empty():
      _session_queues.pop(session_id, None)


async def run_message(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_content: str,
    persist_and_broadcast,
    save_metadata=None,
    mark_unread=None,
    clear_thinking_since=None,
    skip_user_event: bool = False,
    is_voice: bool = False,
    auto_trigger: bool = False,
    backend_option: Optional[BackendOption] = None,
    extra_claude_flags: Optional[list[str]] = None,
) -> Optional[str]:
  """Spawn a Claude Code process for the master agent and stream NDJSON events.

  Args:
    cfg: App configuration.
    session_meta: The session to run in.
    user_content: The user's message text.
    persist_and_broadcast: Coroutine to persist each event (injecting timestamp)
      then broadcast to the session WebSocket channel.
    save_metadata: Coroutine to persist session metadata updates.
    mark_unread: Coroutine to mark the session unread for other viewers.
    clear_thinking_since: Coroutine to clear thinking_since by re-reading fresh
      metadata from disk, avoiding clobbering has_unread.
    skip_user_event: If True, skip persisting/broadcasting the user event
      (used when the master is triggered by a worker completion, not a real user message).

  Returns:
    The CC session ID (for --resume on subsequent messages), or None.
  """
  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)

  tex_path = get_tex_path()
  should_check_tex = tex_path.exists()
  if should_check_tex:
    await asyncio.to_thread(snapshot_tex)

  # Persist the user message so it survives page refresh (WebSocket catch-up)
  if not skip_user_event:
    user_event = {
        "type": ET.USER,
        "content": user_content,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "is_voice": is_voice
    }
    await persist_and_broadcast(session_meta.id, user_event)
    session_meta.updated_at = datetime.now(timezone.utc)

  # If no consumer is running for this session, set thinking state now.
  # If a consumer is already running, thinking is already active.
  consumer_already_running = (session_meta.id in _session_consumers and not _session_consumers[session_meta.id].done())
  if not consumer_already_running:
    session_meta.thinking_since = datetime.now(timezone.utc)
    if save_metadata:
      await save_metadata(session_meta)
    await streaming_manager.broadcast(
        'sidebar', {
            'type': ET.RUNNING_CHANGED,
            'session_id': session_meta.id,
            'has_running_tasks': True,
            'auto_trigger': auto_trigger,
        })
  else:
    # Still save metadata even when consumer is already running
    # (e.g. updated_at changed above).
    if save_metadata:
      await save_metadata(session_meta)

  # Create a future for the caller to await.
  loop = asyncio.get_running_loop()
  future: asyncio.Future = loop.create_future()

  work_item = _WorkItem(
      cfg=cfg,
      session_meta=session_meta,
      user_content=user_content,
      persist_and_broadcast=persist_and_broadcast,
      save_metadata=save_metadata,
      mark_unread=mark_unread,
      clear_thinking_since=clear_thinking_since,
      is_voice=is_voice,
      auto_trigger=auto_trigger,
      backend_option=backend_option,
      extra_claude_flags=extra_claude_flags,
      should_check_tex=should_check_tex,
      future=future,
  )

  # Enqueue the work item.
  if session_meta.id not in _session_queues:
    _session_queues[session_meta.id] = asyncio.Queue()
  _session_queues[session_meta.id].put_nowait(work_item)

  # Ensure a consumer task is running for this session.
  if session_meta.id not in _session_consumers or _session_consumers[session_meta.id].done():
    _session_consumers[session_meta.id] = asyncio.create_task(
        _session_consumer(session_meta.id),
        name=f"master-consumer-{session_meta.id[:8]}",
    )

  # Await until this specific work item completes.
  return await future


async def cancel_master(session_id: str) -> bool:
  """Terminate the running master CC backend for this session.

  Returns True if a backend was found and terminate() was called, False otherwise.
  """
  log.info("master_cancel_requested", session=session_id)
  backend = _active_procs.get(session_id)
  if backend is None:
    log.info("master_cancel_no_active_master", session=session_id)
    return False
  await backend.terminate()
  log.info("master_cancel_succeeded", session=session_id)
  return True
