"""Auto-name sessions after the first chat turn using Gemini."""

import re

import structlog

from src.agents.gemini_provider import GeminiProvider
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager

log = structlog.get_logger()

_DEFAULT_NAME_RE = re.compile(r"^(Session \d+$|\d+: )")
_SESSION_NUMBER_RE = re.compile(r"^Session (\d+)$")

_NAMING_PROMPT = (
    "Generate a short, descriptive title (3-6 words) for this conversation. "
    "Return ONLY the title, no quotes, no punctuation at the end, no explanation.\n\n"
    "User: {user_message}\n\n"
    "Assistant: {assistant_response}")


async def maybe_auto_name(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_message: str,
    assistant_response: str,
    session_mgr: SessionManager,
) -> None:
  """If the session still has a default name, generate a descriptive one via Gemini."""
  if not _DEFAULT_NAME_RE.match(session_meta.name):
    return

  if not cfg.gemini_api_key:
    log.debug("autonamer_skipped_no_api_key")
    return

  try:
    prompt = _NAMING_PROMPT.format(
        user_message=user_message[:500],
        assistant_response=assistant_response[:1000],
    )

    provider = GeminiProvider(api_key=cfg.gemini_api_key, model=cfg.gemini_model)
    raw = await provider.generate_text(prompt)

    # Sanitize: strip quotes, limit length
    name = raw.strip().strip('"\'').strip()
    if not name:
      return
    if len(name) > 60:
      name = name[:57] + "..."

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
