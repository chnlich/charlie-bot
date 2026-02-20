"""Master CC — spawns a Claude Code subprocess for the master agent."""

import asyncio
import json
import os
from pathlib import Path
from typing import Optional

import structlog

from src.core.config import CharliBotConfig
from src.core.models import SessionMetadata
from src.core.streaming import streaming_manager

log = structlog.get_logger()

MASTER_CC_COMMAND = [
  "claude",
  "-p",
  "--output-format",
  "stream-json",
  "--verbose",
  "--dangerously-skip-permissions",
]

_MASTER_TEMPLATE_PATH = Path(__file__).parent.parent.parent / "config" / "master-claude.md"


def _ensure_master_claude_md(session_meta: SessionMetadata, cfg: CharliBotConfig) -> None:
  """Write the master CLAUDE.md into ~/.charliebot/ and symlink into the session dir."""
  claude_md = cfg.claude_md_file

  # Only write if there's no CLAUDE.md yet (don't clobber user's)
  if not claude_md.exists():
    if not _MASTER_TEMPLATE_PATH.exists():
      log.warning("master_claude_md_template_missing", path=str(_MASTER_TEMPLATE_PATH))
      return

    template = _MASTER_TEMPLATE_PATH.read_text(encoding="utf-8")
    content = template.replace("{session_id}", session_meta.id)
    claude_md.write_text(content, encoding="utf-8")
    log.info("master_claude_md_written", path=str(claude_md))

  # Ensure symlink in session directory
  symlink = cfg.session_claude_md_symlink(session_meta.id)
  if not symlink.exists():
    symlink.parent.mkdir(parents=True, exist_ok=True)
    symlink.symlink_to(os.path.relpath(claude_md, symlink.parent))
    log.info("master_claude_md_symlinked", link=str(symlink), target=str(claude_md))


async def run_message(
  cfg: CharliBotConfig,
  session_meta: SessionMetadata,
  user_content: str,
  save_chat_event,
) -> Optional[str]:
  """Spawn a Claude Code process for the master agent and stream NDJSON events.

  Args:
    cfg: App configuration.
    session_meta: The session to run in.
    user_content: The user's message text.
    save_chat_event: Coroutine to persist each event to chat_events.jsonl.

  Returns:
    The CC session ID (for --resume on subsequent messages), or None.
  """
  channel = f"session:{session_meta.id}"
  session_dir = cfg.sessions_dir / session_meta.id
  session_dir.mkdir(parents=True, exist_ok=True)
  cwd = str(session_dir)

  # Ensure master CLAUDE.md exists in the working directory
  _ensure_master_claude_md(session_meta, cfg)

  # Persist the user message so it survives page refresh (WebSocket catch-up)
  user_event = {"type": "user", "content": user_content}
  await save_chat_event(session_meta.id, user_event)
  await streaming_manager.broadcast(channel, user_event)

  cmd = list(MASTER_CC_COMMAND)
  if session_meta.cc_session_id:
    cmd += ["--resume", session_meta.cc_session_id]
  cmd.append(user_content)

  env = {**os.environ}
  env.pop("CLAUDECODE", None)

  log.info("master_cc_starting", session=session_meta.id, cwd=cwd)

  proc = await asyncio.create_subprocess_exec(
    *cmd,
    cwd=cwd,
    stdout=asyncio.subprocess.PIPE,
    stderr=asyncio.subprocess.PIPE,
    env=env,
  )

  log.info("master_cc_spawned", session=session_meta.id, pid=proc.pid)

  cc_session_id: Optional[str] = session_meta.cc_session_id

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

  # Emit master_done so the frontend knows the response is complete
  done_event = {"type": "master_done", "exit_code": exit_code}
  await streaming_manager.broadcast(channel, done_event)
  await save_chat_event(session_meta.id, done_event)

  log.info("master_cc_finished", session=session_meta.id, exit_code=exit_code)

  return cc_session_id
