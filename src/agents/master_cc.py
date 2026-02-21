"""Master CC — spawns a Claude Code subprocess for the master agent."""

import asyncio
import json
import os
from datetime import datetime, timezone
from typing import Optional

import structlog

from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata
from src.core.streaming import streaming_manager

log = structlog.get_logger()

# Per-session counter of concurrently running run_message tasks.
# Only clear thinking_since when the count drops to zero.
_active_tasks: dict[str, int] = {}

MASTER_CC_COMMAND = [
  "claude",
  "-p",
  "--output-format",
  "stream-json",
  "--verbose",
  "--dangerously-skip-permissions",
]


def ensure_master_claude_md(session_meta: SessionMetadata, cfg: CharlieBotConfig) -> None:
  """Write session CLAUDE.md by concatenating MASTER_AGENT_PROMPT.md + MEMORY.md."""
  prompt_file = cfg.claude_md_file

  # Concatenate prompt + memory → session CLAUDE.md (rewritten each time)
  parts = [prompt_file.read_text(encoding="utf-8")]
  if cfg.memory_file.exists():
    parts.append(cfg.memory_file.read_text(encoding="utf-8"))

  session_claude_md = cfg.session_claude_md(session_meta.id)
  session_claude_md.parent.mkdir(parents=True, exist_ok=True)

  # Remove stale symlink (from pre-refactor sessions) so we write a real file
  if session_claude_md.is_symlink():
    session_claude_md.unlink()

  session_claude_md.write_text("\n\n".join(parts), encoding="utf-8")
  log.debug("session_claude_md_written", path=str(session_claude_md))


async def run_message(
  cfg: CharlieBotConfig,
  session_meta: SessionMetadata,
  user_content: str,
  save_chat_event,
  save_metadata=None,
  mark_unread=None,
  skip_user_event: bool = False,
) -> Optional[str]:
  """Spawn a Claude Code process for the master agent and stream NDJSON events.

  Args:
    cfg: App configuration.
    session_meta: The session to run in.
    user_content: The user's message text.
    save_chat_event: Coroutine to persist each event to chat_events.jsonl.
    save_metadata: Coroutine to persist session metadata updates.
    skip_user_event: If True, skip persisting/broadcasting the user event
      (used when the master is triggered by a worker completion, not a real user message).

  Returns:
    The CC session ID (for --resume on subsequent messages), or None.
  """
  channel = f"session:{session_meta.id}"
  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)
  cwd = str(session_dir)

  # Write session CLAUDE.md (prompt + memory) so Claude Code picks it up
  ensure_master_claude_md(session_meta, cfg)

  # Persist the user message so it survives page refresh (WebSocket catch-up)
  if not skip_user_event:
    user_event = {"type": "user", "content": user_content, "timestamp": datetime.now(timezone.utc).isoformat()}
    await save_chat_event(session_meta.id, user_event)
    await streaming_manager.broadcast(channel, user_event)
    session_meta.updated_at = datetime.now(timezone.utc)

  # Track concurrent tasks; only set thinking_since on the first one
  _active_tasks[session_meta.id] = _active_tasks.get(session_meta.id, 0) + 1
  if _active_tasks[session_meta.id] == 1:
    session_meta.thinking_since = datetime.now(timezone.utc)
  if save_metadata:
    await save_metadata(session_meta)

  cmd = list(MASTER_CC_COMMAND)
  if session_meta.cc_session_id:
    cmd += ["--resume", session_meta.cc_session_id]
  cmd.append(user_content)

  env = {**os.environ}
  env.pop("CLAUDECODE", None)
  env["GIT_CEILING_DIRECTORIES"] = str(cfg.charliebot_home)

  log.info("master_cc_starting", session=session_meta.id, cwd=cwd)

  cc_session_id: Optional[str] = session_meta.cc_session_id
  exit_code = 1
  error_msg: Optional[str] = None

  try:
    proc = await asyncio.create_subprocess_exec(
      *cmd,
      cwd=cwd,
      stdin=asyncio.subprocess.DEVNULL,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
      env=env,
      limit=cfg.subprocess_buffer_limit,
    )

    log.info("master_cc_spawned", session=session_meta.id, pid=proc.pid)

    assert proc.stdout is not None
    async for raw_line in proc.stdout:
      line = raw_line.decode("utf-8", errors="replace").strip()
      if not line:
        continue
      try:
        event = json.loads(line)
      except json.JSONDecodeError as e:
        log.debug("master_cc_line_not_json", error=str(e))
        continue

      # Capture the CC session ID from the first event that has one
      if not cc_session_id:
        sid = event.get("session_id")
        if sid:
          cc_session_id = sid

      # Broadcast to WebSocket subscribers and persist
      await streaming_manager.broadcast(channel, event)
      await save_chat_event(session_meta.id, event)

    # Read stderr
    assert proc.stderr is not None
    stderr_bytes = await proc.stderr.read()
    await proc.wait()
    exit_code = proc.returncode or 0

    if stderr_bytes:
      stderr_text = stderr_bytes.decode("utf-8", errors="replace").strip()
      if stderr_text:
        log.warning("master_cc_stderr", session=session_meta.id, stderr=stderr_text[:500])
        if exit_code != 0:
          error_msg = stderr_text[:500]

  except Exception as e:
    log.error("master_cc_crashed", session=session_meta.id, error=str(e))
    error_msg = str(e)

  finally:
    # Decrement active-task counter; only clear thinking when ALL tasks finish
    _active_tasks[session_meta.id] = max(_active_tasks.get(session_meta.id, 1) - 1, 0)
    still_thinking = _active_tasks.get(session_meta.id, 0) > 0

    if not still_thinking:
      session_meta.thinking_since = None
      if save_metadata:
        await save_metadata(session_meta)

    if error_msg:
      err_event = {"type": "assistant_error", "content": f"Agent error: {error_msg}"}
      await streaming_manager.broadcast(channel, err_event)
      await save_chat_event(session_meta.id, err_event)

    # Mark session unread so other viewers see the new output
    if mark_unread:
      await mark_unread(session_meta.id)

    done_event = {"type": "master_done", "exit_code": exit_code, "still_thinking": still_thinking}
    await streaming_manager.broadcast(channel, done_event)
    await save_chat_event(session_meta.id, done_event)

    log.info("master_cc_finished", session=session_meta.id, exit_code=exit_code, still_thinking=still_thinking)

  return cc_session_id
