"""Master CC — spawns a Claude Code subprocess for the master agent."""

import asyncio
import os
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

# Per-session counter of concurrently running run_message tasks.
# Only clear thinking_since when the count drops to zero.
_active_tasks: dict[str, int] = {}

# Per-session running backend reference for external cancellation.
_active_procs: dict[str, AgentBackend] = {}


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


async def _finalize_session(
    session_meta: SessionMetadata,
    exit_code: int,
    error_msg: Optional[str],
    should_check_tex: bool,
    persist_and_broadcast,
    save_metadata=None,
    mark_unread=None,
    auto_trigger: bool = False,
) -> None:
  """Clean up after run_message: update thinking state, broadcast errors/done, check tex."""
  _active_procs.pop(session_meta.id, None)
  # Decrement active-task counter; only clear thinking when ALL tasks finish
  _active_tasks[session_meta.id] = max(_active_tasks.get(session_meta.id, 1) - 1, 0)
  still_thinking = _active_tasks.get(session_meta.id, 0) > 0

  thinking_seconds = None
  if not still_thinking:
    if session_meta.thinking_since:
      thinking_seconds = int((datetime.now(timezone.utc) - session_meta.thinking_since).total_seconds())
    session_meta.thinking_since = None
    if save_metadata:
      await save_metadata(session_meta)
    await streaming_manager.broadcast(
        "sidebar", {
            "type": ET.RUNNING_CHANGED,
            "session_id": session_meta.id,
            "has_running_tasks": False,
            "auto_trigger": auto_trigger,
        })

  if error_msg:
    err_event = {"type": ET.ASSISTANT_ERROR, "content": f"Agent error: {error_msg}"}
    await persist_and_broadcast(session_meta.id, err_event)

  # Mark session unread so other viewers see the new output
  if mark_unread:
    await mark_unread(session_meta.id)

  done_event = {"type": ET.MASTER_DONE, "exit_code": exit_code, "still_thinking": still_thinking}
  if thinking_seconds is not None:
    done_event["thinking_seconds"] = thinking_seconds
  await persist_and_broadcast(session_meta.id, done_event)

  if should_check_tex:
    proposal = await asyncio.to_thread(check_tex_changed)
    if proposal:
      tex_event = {'type': ET.TEX_EDIT_PROPOSED}
      await persist_and_broadcast(session_meta.id, tex_event)
      log.info('tex_edit_proposed', session=session_meta.id)
    else:
      clear_snapshot()

  log.info("master_cc_finished", session=session_meta.id, exit_code=exit_code, still_thinking=still_thinking)


async def run_message(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_content: str,
    persist_and_broadcast,
    save_metadata=None,
    mark_unread=None,
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
    skip_user_event: If True, skip persisting/broadcasting the user event
      (used when the master is triggered by a worker completion, not a real user message).

  Returns:
    The CC session ID (for --resume on subsequent messages), or None.
  """
  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)
  cwd = str(session_dir)

  # Build instructions content in memory (all backends receive it uniformly)
  instructions_content = await asyncio.to_thread(_build_instructions_content, session_meta, cfg)

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

  # Track concurrent tasks; only set thinking_since on the first one.
  # All runs (including auto-triggered) participate in thinking state so the
  # sidebar correctly shows the session as running.  The auto_trigger flag is
  # forwarded to the frontend so it can keep the send button enabled during
  # background processing (user can still type while auto-triggered master works).
  _active_tasks[session_meta.id] = _active_tasks.get(session_meta.id, 0) + 1
  if _active_tasks[session_meta.id] == 1:
    session_meta.thinking_since = datetime.now(timezone.utc)
  if save_metadata:
    await save_metadata(session_meta)
  if _active_tasks[session_meta.id] == 1:
    await streaming_manager.broadcast(
        'sidebar', {
            'type': ET.RUNNING_CHANGED,
            'session_id': session_meta.id,
            'has_running_tasks': True,
            'auto_trigger': auto_trigger,
        })

  from src.agents.backends.registry import build_backend
  option = backend_option or cfg.backend_options[0]

  extra_flags: list[str] = []
  if session_meta.cc_session_id and option.type not in ("codex", "gemini", "opencode"):
    extra_flags = ["--resume", session_meta.cc_session_id]
  if extra_claude_flags:
    extra_flags.extend(extra_claude_flags)

  env = {**os.environ}
  env.pop("CLAUDECODE", None)
  env["GIT_CEILING_DIRECTORIES"] = str(cfg.charliebot_home)
  env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"

  log.info("master_cc_starting", session=session_meta.id, cwd=cwd)

  cc_session_id: Optional[str] = session_meta.cc_session_id
  exit_code = 1
  error_msg: Optional[str] = None

  async def _on_spawn(pid: int) -> None:
    log.info("master_cc_spawned", session=session_meta.id, pid=pid)

  backend: Optional[AgentBackend] = None

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

    prompt = _build_prompt(user_content, is_voice)

    async for event in backend.run(prompt, cwd, env):
      cc_session_id = await _handle_event(event, session_meta.id, cc_session_id, persist_and_broadcast)

    exit_code = backend.exit_code
    if backend.stderr_text:
      log.warning("master_cc_stderr", session=session_meta.id, stderr=backend.stderr_text)
      if exit_code != 0:
        error_msg = backend.stderr_text[:500]

  except Exception as e:
    log.exception("master_cc_crashed", session=session_meta.id)
    error_msg = str(e)

  finally:
    await _finalize_session(
        session_meta,
        exit_code,
        error_msg,
        should_check_tex,
        persist_and_broadcast,
        save_metadata=save_metadata,
        mark_unread=mark_unread,
        auto_trigger=auto_trigger,
    )

  return cc_session_id


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
