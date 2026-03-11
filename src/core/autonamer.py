"""Auto-name sessions after the first chat turn using Gemini or Claude CLI."""

import asyncio
import re

import structlog

from src.agents.gemini_provider import GeminiProvider
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager

log = structlog.get_logger()

# Matches true defaults ("Session 7") and legacy empty placeholders ("7: ").
# Does NOT match already-renamed titles like "7: My Topic".
_DEFAULT_NAME_RE = re.compile(r"^(Session \d+|\d+: )$")
_SESSION_NUMBER_RE = re.compile(r"^Session (\d+)$")
_MARKDOWN_CHARS_RE = re.compile(r"[*`#_~\[\]]")
_PREAMBLE_RE = re.compile(
    r"^(here(?:'s| is|are)\s.*?[:]\s*|sure[,!.\s]+|title:\s*|okay[,.\s]+)",
    re.IGNORECASE,
)
_MAX_TITLE_WORDS = 8


def is_default_session_name(name: str) -> bool:
  """Return True if *name* is a system-generated default (not yet user/auto-named)."""
  return bool(_DEFAULT_NAME_RE.match(name))


_NAMING_PROMPT = (
    "Generate a short, descriptive title (3-6 words) for this conversation. "
    "Return ONLY the title, no quotes, no punctuation at the end, no explanation.\n\n"
    "User: {user_message}\n\n"
    "Assistant: {assistant_response}")


async def _generate_name_via_claude_cli(prompt: str) -> str:
  """Generate text using the claude CLI in print mode."""
  proc = await asyncio.create_subprocess_exec(
      "claude",
      "-p",
      "--output-format",
      "text",
      "--no-session-persistence",
      "--model",
      "sonnet",
      prompt,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
  )
  stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=30.0)
  if proc.returncode != 0:
    raise RuntimeError(f"claude CLI failed (exit {proc.returncode}): {stderr.decode().strip()}")
  return stdout.decode().strip()


async def maybe_auto_name(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_message: str,
    assistant_response: str,
    session_mgr: SessionManager,
) -> None:
  """If the session still has a default name, generate a descriptive one."""
  if not is_default_session_name(session_meta.name):
    return

  try:
    prompt = _NAMING_PROMPT.format(
        user_message=user_message[:500],
        assistant_response=assistant_response[:1000],
    )

    if cfg.gemini_api_key:
      provider = GeminiProvider(api_key=cfg.gemini_api_key, model=cfg.gemini_model)
      raw = await provider.generate_text(prompt)
    else:
      raw = await _generate_name_via_claude_cli(prompt)

    # Sanitize: extract a concise title from potentially verbose LLM output
    # 1. Take only the first non-empty line
    name = ""
    for line in raw.splitlines():
      line = line.strip()
      if line:
        name = line
        break
    if not name:
      return

    # 2. Strip quotes and markdown formatting
    name = name.strip('"\'').strip()
    name = _MARKDOWN_CHARS_RE.sub("", name)
    name = name.strip()

    # 3. Strip common LLM preamble patterns
    name = _PREAMBLE_RE.sub("", name).strip()

    if not name:
      return

    # 4. Discard if still too long or looks like a sentence
    if len(name) > 60 or len(name.split()) > _MAX_TITLE_WORDS:
      return

    # Prefix with session number extracted from 'Session N'
    m = _SESSION_NUMBER_RE.match(session_meta.name)
    if m:
      name = f"{m.group(1)}: {name}"

    await session_mgr.rename_session(session_meta.id, name)

    channel = f"session:{session_meta.id}"
    await streaming_manager.broadcast(channel, {
        "type": "session_renamed",
        "name": name,
    })
    await streaming_manager.broadcast(
        "sidebar", {
            "type": "session_renamed",
            "session_id": session_meta.id,
            "name": name,
        })

    log.info("session_auto_named", session_id=session_meta.id, name=name)

  except Exception as e:
    log.warning("autonamer_failed", session_id=session_meta.id, error=str(e))
