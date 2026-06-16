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
from src.core.models import BackendOption, SessionCallbacks, SessionMetadata, backend_type_allows_missing_model
from src.core.streaming import handle_compact_boundary, streaming_manager

log = structlog.get_logger()


class _RunTimingTracker:
  """Tracks monotonic timing milestones during a single _run_cc execution."""

  def __init__(self, session_id: str, backend_type: str, model: Optional[str]) -> None:
    self._session_id = session_id
    self._backend_type = backend_type
    self._model = model
    self._t_start = time.monotonic()
    self._t_spawn: Optional[float] = None
    self._t_first_event: Optional[float] = None
    self._t_first_assistant: Optional[float] = None
    self._saw_first_assistant = False

  async def on_spawn(self, pid: int) -> None:
    self._t_spawn = time.monotonic()
    log.info("master_cc_spawned", session=self._session_id, pid=pid, backend=self._backend_type, model=self._model)

  def on_event(self, event: dict) -> None:
    if self._t_first_event is None:
      self._t_first_event = time.monotonic()
      spawn_ref = self._t_spawn if self._t_spawn is not None else self._t_start
      spawn_to_first_ms = int((self._t_first_event - spawn_ref) * 1000)
      log.info(
          "master_cc_first_event",
          session=self._session_id,
          event_type=event.get("type"),
          spawn_to_first_event_ms=spawn_to_first_ms,
      )
      if spawn_to_first_ms > 10_000:
        log.warning(
            "master_cc_slow_first_event",
            session=self._session_id,
            spawn_to_first_event_ms=spawn_to_first_ms,
        )

    if not self._saw_first_assistant and event.get("type") == ET.ASSISTANT:
      msg = event.get("message", {})
      content_blocks = msg.get("content") if isinstance(msg, dict) else None
      if content_blocks:
        for block in content_blocks:
          if isinstance(block, dict) and block.get("type") == "text" and block.get("text"):
            self._saw_first_assistant = True
            self._t_first_assistant = time.monotonic()
            log.info(
                "master_cc_first_assistant_text",
                session=self._session_id,
                first_assistant_ms=int((self._t_first_assistant - self._t_start) * 1000),
            )
            break

  def build_finish_extras(self) -> dict:
    total_ms = int((time.monotonic() - self._t_start) * 1000)
    extras: dict = {
        "backend": self._backend_type,
        "model": self._model,
        "total_ms": total_ms,
    }
    if self._t_first_event is not None:
      spawn_ref = self._t_spawn if self._t_spawn is not None else self._t_start
      extras["spawn_to_first_event_ms"] = int((self._t_first_event - spawn_ref) * 1000)
    if self._t_first_assistant is not None:
      extras["first_assistant_ms"] = int((self._t_first_assistant - self._t_start) * 1000)
    if total_ms > 120_000:
      log.warning("master_cc_slow_total", session=self._session_id, total_ms=total_ms)
    return extras


# Per-session FIFO queue for serializing run_message calls.
_session_queues: dict[str, asyncio.Queue] = {}

# One consumer task per session — drains the queue sequentially.
_session_consumers: dict[str, asyncio.Task] = {}

# Per-session running backend reference for external cancellation.
_active_procs: dict[str, AgentBackend] = {}

_CLAUDE_RESUME_FLAG_BACKEND_TYPES = {"cc-claude", "cc-kimi", "cc-deepseek-sglang"}
_NATIVE_RESUME_SESSION_BACKEND_TYPES = {"codex", "gemini", "opencode"}


@dataclasses.dataclass
class _WorkItem:
  """All arguments needed to execute a single CC run, plus a future for the result."""
  cfg: CharlieBotConfig
  session_meta: SessionMetadata
  user_content: str
  callbacks: SessionCallbacks
  is_voice: bool
  auto_trigger: bool
  backend_option: Optional[BackendOption]
  extra_claude_flags: Optional[list[str]]
  should_check_tex: bool
  future: asyncio.Future


def _build_instructions_content(session_meta: SessionMetadata, cfg: CharlieBotConfig) -> Optional[str]:
  """Build master agent instructions by concatenating base prompt + per-host override + memory files."""
  parts: list[str] = []

  # 1. Git-shared base prompt (prompts/master.md in the repo)
  base_prompt_file = cfg.charlie_bot_repo / "prompts" / "master.md"
  if base_prompt_file.exists():
    base_text = base_prompt_file.read_text(encoding="utf-8")
    base_text = base_text.replace("{{session_id}}", session_meta.id)
    parts.append(base_text)

  # 2. Per-host override (~/.charliebot/MASTER_AGENT_PROMPT.md)
  host_prompt_file = cfg.claude_md_file
  if host_prompt_file.exists():
    host_text = host_prompt_file.read_text(encoding="utf-8")
    host_text = host_text.replace("YOUR_SESSION_UUID", session_meta.id)
    parts.append(host_text)

  if not parts:
    log.warning("master_prompt_files_missing", base=str(base_prompt_file), host=str(host_prompt_file))
    return None

  # 3. Memory files
  for mf in [cfg.memory_file, cfg.memory_host_file]:
    if mf.exists():
      parts.append(mf.read_text(encoding="utf-8"))

  return "\n\n".join(parts)


def _build_prompt(user_content: str, is_voice: bool) -> str:
  """Prepend voice-transcription disclaimer when the message comes from voice input."""
  if is_voice:
    return (
        "[The following message is from voice transcription and might not be accurate. "
        "Please ask the user first for any words that are unclear or might be wrong.]\n"
        f"{user_content}")
  return user_content


def _route_resume_session(backend_type: str, cc_session_id: Optional[str]) -> tuple[list[str], Optional[str]]:
  """Return CLI resume flags and native resume ID for a backend type."""
  if not cc_session_id:
    return [], None
  if backend_type in _CLAUDE_RESUME_FLAG_BACKEND_TYPES:
    return ["--resume", cc_session_id], None
  if backend_type in _NATIVE_RESUME_SESSION_BACKEND_TYPES:
    return [], cc_session_id
  return [], None


def _build_master_env(cfg: CharlieBotConfig, session_id: str) -> dict[str, str]:
  """Build the environment for the master backend subprocess."""
  env = {**os.environ}
  env.pop("CLAUDECODE", None)
  env["GIT_CEILING_DIRECTORIES"] = str(cfg.charliebot_home)
  env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"
  env["CHARLIEBOT_SESSION_ID"] = session_id

  venv_bin = cfg.charlie_bot_repo / ".venv" / "bin"
  if venv_bin.is_dir():
    existing_path = env.get("PATH")
    env["PATH"] = str(venv_bin) if not existing_path else f"{venv_bin}{os.pathsep}{existing_path}"

  return env


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
  if backend_type_allows_missing_model(option.type) and option.model is not None:
    option = option.model_copy(update={"model": None})

  extra_flags, resume_session_id = _route_resume_session(option.type, session_meta.cc_session_id)
  resume_session = bool(extra_flags or resume_session_id)
  if session_meta.cc_session_id and not resume_session:
    log.warning("master_cc_resume_unsupported_backend", session=session_meta.id, backend=option.type)
  if item.extra_claude_flags:
    extra_flags.extend(item.extra_claude_flags)

  env = _build_master_env(cfg, session_meta.id)

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

  tracker = _RunTimingTracker(session_meta.id, option.type, option.model)
  backend: Optional[AgentBackend] = None

  try:
    backend = build_backend(
        option,
        cfg,
        extra_flags=extra_flags or None,
        buffer_limit=cfg.subprocess_buffer_limit,
        on_spawn=tracker.on_spawn,
        instructions_content=instructions_content,
        resume_session_id=resume_session_id,
    )
    _active_procs[session_meta.id] = backend

    async for event in backend.run(prompt, cwd, env):
      tracker.on_event(event)
      cc_session_id = await _handle_event(event, session_meta.id, cc_session_id, item.callbacks.persist_and_broadcast)

    exit_code = backend.exit_code
    if backend.stderr_text:
      log.warning("master_cc_stderr", session=session_meta.id, stderr=backend.stderr_text)
      if exit_code != 0 and not backend.terminated:
        error_msg = backend.stderr_text[:500]

  except asyncio.CancelledError:
    log.warning("master_cc_cancelled", session=session_meta.id)
    if backend:
      await backend.terminate()
    _active_procs.pop(session_meta.id, None)
    raise
  except Exception as e:
    log.exception("master_cc_crashed", session=session_meta.id)
    error_msg = str(e)

  finally:
    _active_procs.pop(session_meta.id, None)
    finish_extras = tracker.build_finish_extras()

    if error_msg:
      err_event = {"type": ET.ASSISTANT_ERROR, "content": f"Agent error: {error_msg}"}
      await item.callbacks.persist_and_broadcast(session_meta.id, err_event)

    await item.callbacks.mark_unread(session_meta.id)

    if item.should_check_tex:
      proposal = await asyncio.to_thread(check_tex_changed)
      if proposal:
        tex_event = {'type': ET.TEX_EDIT_PROPOSED}
        await item.callbacks.persist_and_broadcast(session_meta.id, tex_event)
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
  # Relay cc_session_id across items: queued _WorkItems may carry distinct
  # SessionMetadata instances (e.g. fork bootstrap vs. user message loaded later).
  last_cc_session_id: Optional[str] = None
  try:
    while True:
      item: _WorkItem = await queue.get()
      try:
        # Carry the previous run's cc_session_id onto a freshly-loaded meta
        # so --resume picks up the in-progress CC transcript.
        if last_cc_session_id and not item.session_meta.cc_session_id:
          item.session_meta.cc_session_id = last_cc_session_id
        result = await _run_cc(item)
        cc_session_id, exit_code, _error_msg, finish_extras = result

        # Update session_meta.cc_session_id for subsequent queued runs.
        if cc_session_id:
          item.session_meta.cc_session_id = cc_session_id
          last_cc_session_id = cc_session_id

        still_thinking = not queue.empty()

        # Broadcast MASTER_DONE
        thinking_seconds = None
        if not still_thinking and item.session_meta.thinking_since:
          thinking_seconds = int((datetime.now(timezone.utc) - item.session_meta.thinking_since).total_seconds())

        done_event = {"type": ET.MASTER_DONE, "exit_code": exit_code, "still_thinking": still_thinking}
        if thinking_seconds is not None:
          done_event["thinking_seconds"] = thinking_seconds
        done_event.update({k: v for k, v in finish_extras.items()})
        await item.callbacks.persist_and_broadcast(session_id, done_event)

        # Re-check the queue after the broadcast await: a new run_message may
        # have enqueued a work item while we were yielding. That caller sees
        # the consumer as "already running" and skips setting thinking_since,
        # so clearing it here would persist a stale "not thinking" state while
        # the next item is about to run.
        if not still_thinking and not queue.empty():
          still_thinking = True

        if not still_thinking:
          item.session_meta.thinking_since = None
          await item.callbacks.clear_thinking_since(session_id, cc_session_id)

          # The clear above contains async disk I/O, during which a new
          # run_message may have enqueued an item (it skips setting
          # thinking_since because the consumer task is still running).
          # Restore thinking_since so the next iteration doesn't run with a
          # stale None on disk.
          if not queue.empty():
            restored = datetime.now(timezone.utc)
            item.session_meta.thinking_since = restored
            await item.callbacks.update_thinking_state(session_id, restored, None)
            await streaming_manager.broadcast(
                "sidebar", {
                    "type": ET.RUNNING_CHANGED,
                    "session_id": session_id,
                    "has_running_tasks": True,
                    "thinking_since": restored.isoformat(),
                    "auto_trigger": item.auto_trigger,
                })
          else:
            # Check if workers are still running before declaring idle.
            from src.core.sessions import SessionManager
            workers_running = await SessionManager(item.cfg)._has_running_tasks(session_id)
            await streaming_manager.broadcast(
                "sidebar", {
                    "type": ET.RUNNING_CHANGED,
                    "session_id": session_id,
                    "has_running_tasks": workers_running,
                    "thinking_since": None,
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
    callbacks: SessionCallbacks,
    skip_user_event: bool = False,
    is_voice: bool = False,
    auto_trigger: bool = False,
    backend_option: Optional[BackendOption] = None,
    extra_claude_flags: Optional[list[str]] = None,
    display_content: Optional[str] = None,
    uploaded_files: Optional[list[dict]] = None,
) -> Optional[str]:
  """Spawn a Claude Code process for the master agent and stream NDJSON events.

  Args:
    cfg: App configuration.
    session_meta: The session to run in.
    user_content: The user's message text.
    callbacks: Bundle of session hooks (persist_and_broadcast,
      update_thinking_state, mark_unread, clear_thinking_since).
    skip_user_event: If True, skip persisting/broadcasting the user event
      (used when the master is triggered by a worker completion, not a real user message).
    display_content: User-visible content persisted to the chat log. Defaults
      to ``user_content`` when omitted.
    uploaded_files: Structured uploaded-file metadata persisted on the user event.

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

  # Persist the user message so it survives page refresh (WebSocket catch-up)
  if not skip_user_event:
    user_event = {
        "type": ET.USER,
        "content": user_content if display_content is None else display_content,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "is_voice": is_voice
    }
    if uploaded_files:
      user_event["uploaded_files"] = uploaded_files
    await callbacks.persist_and_broadcast(session_meta.id, user_event)
    session_meta.updated_at = datetime.now(timezone.utc)

  # If no consumer is running for this session, set thinking state now.
  # If a consumer is already running, thinking is already active.
  consumer_already_running = (session_meta.id in _session_consumers and not _session_consumers[session_meta.id].done())
  if not consumer_already_running:
    session_meta.thinking_since = datetime.now(timezone.utc)
    await callbacks.update_thinking_state(session_meta.id, session_meta.thinking_since, session_meta.updated_at)
    await streaming_manager.broadcast(
        'sidebar', {
            'type': ET.RUNNING_CHANGED,
            'session_id': session_meta.id,
            'has_running_tasks': True,
            'thinking_since': session_meta.thinking_since.isoformat(),
            'auto_trigger': auto_trigger,
        })
  else:
    # Still save metadata even when consumer is already running
    # (e.g. updated_at changed above).
    await callbacks.update_thinking_state(session_meta.id, updated_at=session_meta.updated_at)

  # Create a future for the caller to await.
  loop = asyncio.get_running_loop()
  future: asyncio.Future = loop.create_future()

  work_item = _WorkItem(
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
