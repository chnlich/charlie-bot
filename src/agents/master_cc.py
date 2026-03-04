"""Master CC — spawns a Claude Code subprocess for the master agent."""

import asyncio
import os
from datetime import datetime, timezone
from typing import Optional

import structlog

from src.agents.backends.base import AgentBackend
from src.core.config import CharlieBotConfig
from src.core.latex import check_tex_changed, clear_snapshot, get_tex_path, snapshot_tex
from src.core.models import BackendOption, SessionMetadata
from src.core.streaming import streaming_manager

log = structlog.get_logger()

# Per-session counter of concurrently running run_message tasks.
# Only clear thinking_since when the count drops to zero.
_active_tasks: dict[str, int] = {}

# Per-session running backend reference for external cancellation.
_active_procs: dict[str, AgentBackend] = {}


def _build_instructions_content(session_meta: SessionMetadata, cfg: CharlieBotConfig) -> Optional[str]:
  """Build master agent instructions by concatenating MASTER_AGENT_PROMPT.md + MEMORY.md in memory."""
  prompt_file = cfg.claude_md_file
  if not prompt_file.exists():
    log.warning("master_prompt_file_missing", path=str(prompt_file))
    return None

  prompt_text = prompt_file.read_text(encoding="utf-8")
  prompt_text = prompt_text.replace("YOUR_SESSION_UUID", session_meta.id)
  parts = [prompt_text]
  if cfg.memory_file.exists():
    parts.append(cfg.memory_file.read_text(encoding="utf-8"))

  if session_meta.rewind_summary:
    parts.append(
        f"""# Session Rewind Context

This session was rewound from a previous conversation. Here is the conversation summary up to the rewind point:

{session_meta.rewind_summary}

Continue from this context. The user wants to take a different direction from this point.""")

  return "\n\n".join(parts)


async def run_message(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_content: str,
    save_chat_event,
    save_metadata=None,
    mark_unread=None,
    skip_user_event: bool = False,
    is_voice: bool = False,
    backend_option: Optional[BackendOption] = None,
    extra_claude_flags: Optional[list[str]] = None,
) -> Optional[str]:
  """Spawn a Claude Code process for the master agent and stream NDJSON events.

  Args:
    cfg: App configuration.
    session_meta: The session to run in.
    user_content: The user's message text.
    save_chat_event: Coroutine to persist each event to chat_events.jsonl.
    save_metadata: Coroutine to persist session metadata updates.
    mark_unread: Coroutine to mark the session unread for other viewers.
    skip_user_event: If True, skip persisting/broadcasting the user event
      (used when the master is triggered by a worker completion, not a real user message).

  Returns:
    The CC session ID (for --resume on subsequent messages), or None.
  """
  channel = f"session:{session_meta.id}"
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
        "type": "user",
        "content": user_content,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "is_voice": is_voice
    }
    await save_chat_event(session_meta.id, user_event)
    await streaming_manager.broadcast(channel, user_event)
    session_meta.updated_at = datetime.now(timezone.utc)

  # Track concurrent tasks; only set thinking_since on the first one
  _active_tasks[session_meta.id] = _active_tasks.get(session_meta.id, 0) + 1
  if _active_tasks[session_meta.id] == 1:
    session_meta.thinking_since = datetime.now(timezone.utc)
  if save_metadata:
    await save_metadata(session_meta)
  if _active_tasks[session_meta.id] == 1:
    await streaming_manager.broadcast(
        'sidebar', {
            'type': 'running_changed',
            'session_id': session_meta.id,
            'has_running_tasks': True,
        })

  from src.agents.backends.registry import build_backend
  option = backend_option or cfg.backend_options[0]

  extra_flags: list[str] = []
  if session_meta.cc_session_id and option.type not in ("codex", "gemini"):
    extra_flags = ["--resume", session_meta.cc_session_id]
  if extra_claude_flags:
    extra_flags.extend(extra_claude_flags)

  env = {**os.environ}
  env.pop("CLAUDECODE", None)
  env["GIT_CEILING_DIRECTORIES"] = str(cfg.charliebot_home)

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
        resume_session_id=session_meta.cc_session_id if option.type in ("codex", "gemini") else None,
    )
    _active_procs[session_meta.id] = backend

    prompt = user_content
    if is_voice:
      prompt = (
          "[The following message is from voice transcription and might not be accurate. "
          "Please ask the user first for any words that are unclear or might be wrong.]\n"
          f"{user_content}")

    async for event in backend.run(prompt, cwd, env):
      # Capture the CC session ID from the first event that has one
      if not cc_session_id:
        sid = event.get("session_id")
        if sid:
          cc_session_id = sid

      # Persist first (injects timestamp), then broadcast with timestamp included
      await save_chat_event(session_meta.id, event)
      await streaming_manager.broadcast(channel, event)

      if event.get("type") == "system" and event.get("subtype") == "compact_boundary":
        meta = event.get("compact_metadata", {})
        trigger = meta.get("trigger", "unknown")
        pre_tokens = meta.get("pre_tokens")
        log.info("cc_context_compacted", session=session_meta.id, trigger=trigger, pre_tokens=pre_tokens)
        compact_event = {
            "type": "context_compacted",
            "trigger": trigger,
            "pre_tokens": pre_tokens,
        }
        await save_chat_event(session_meta.id, compact_event)
        await streaming_manager.broadcast(channel, compact_event)

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
              "type": "running_changed",
              "session_id": session_meta.id,
              "has_running_tasks": False,
          })

    if error_msg:
      err_event = {"type": "assistant_error", "content": f"Agent error: {error_msg}"}
      await save_chat_event(session_meta.id, err_event)
      await streaming_manager.broadcast(channel, err_event)

    # Mark session unread so other viewers see the new output
    if mark_unread:
      await mark_unread(session_meta.id)

    done_event = {"type": "master_done", "exit_code": exit_code, "still_thinking": still_thinking}
    if thinking_seconds is not None:
      done_event["thinking_seconds"] = thinking_seconds
    await save_chat_event(session_meta.id, done_event)
    await streaming_manager.broadcast(channel, done_event)

    if should_check_tex:
      proposal = await asyncio.to_thread(check_tex_changed)
      if proposal:
        tex_event = {'type': 'tex_edit_proposed'}
        await save_chat_event(session_meta.id, tex_event)
        await streaming_manager.broadcast(channel, tex_event)
        log.info('tex_edit_proposed', session=session_meta.id)
      else:
        clear_snapshot()

    log.info("master_cc_finished", session=session_meta.id, exit_code=exit_code, still_thinking=still_thinking)

  return cc_session_id


async def cancel_master(session_id: str) -> bool:
  """Terminate the running master CC backend for this session.

  Returns True if a backend was found and terminate() was called, False otherwise.
  """
  backend = _active_procs.get(session_id)
  if backend is None:
    return False
  await backend.terminate()
  return True
