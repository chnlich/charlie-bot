"""In-session recap: cheap pure-extraction plus an opt-in cached light-backend summary.

The default path is zero-token. ``extract_recap`` scans a session's (sparse)
user events and returns the ordered asks plus the last exchange — no LLM call.
A concise summary is generated only on an explicit request (via a same-type light
backend from model_preference) and cached per ``(session_id, upto)`` so reopening
an unchanged divider costs nothing.
"""

import asyncio
import json
from pathlib import Path

import structlog

from src.agents.backends.registry import build_backend
from src.api.message_utils import events_to_messages
from src.core.autonamer import select_light_backend
from src.core.config import CharlieBotConfig
from src.core.models import utc_now
from src.core.sessions import SessionManager
from src.core.timeouts import AUTONAMER_TIMEOUT

log = structlog.get_logger()

_ASK_CHARS = 80
_LAST_CHARS = 250

# User messages auto-injected by the system are not real "asks". Matched by prefix
# against the trigger banner and the fork/elone bootstrap prompts.
_AUTO_INJECTED_PREFIXES = (
    "[Scheduled trigger fired",
    "This session was cloned from a previous conversation.",
    "This session continues a prior conversation.",
    "You're taking over a task from a previous session where user wasn't satisfied.",
    "You're taking over because the user wasn't satisfied with the previous session.",
)

_SUMMARY_SYSTEM_PROMPT = (
    "You are a session recap assistant. Given the user's list of asks and the last "
    "exchange of a conversation, produce a concise recap: first a few bullet points "
    "describing what was discussed, then one line describing what was last being worked "
    "on. Do not restate the input verbatim, and add no pleasantries or explanations. "
    "Write the recap in the SAME language as the conversation content you are given "
    "below (the asks and the last exchange): a Chinese conversation gets a Chinese "
    "recap, an English conversation gets an English recap. Do not default to any fixed "
    "output language.")

_SUMMARY_PROMPT = (
    "User asks (in chronological order):\n{asks}\n\n"
    "Last exchange:\nUser: {last_user}\nAssistant: {last_assistant}\n\n"
    "Produce the recap:\n- a few bullet points for what was discussed\n"
    "- one line for what was last being worked on")


def _truncate(text: str, limit: int) -> str:
  """Strip and clip *text* to *limit* chars, appending an ellipsis if clipped."""
  text = (text or "").strip()
  return text if len(text) <= limit else text[:limit].rstrip() + "…"


def _first_line(text: str) -> str:
  """Return the first non-blank line of *text*."""
  for line in (text or "").splitlines():
    stripped = line.strip()
    if stripped:
      return stripped
  return ""


def _is_auto_injected(content: str) -> bool:
  """True if *content* is a system-injected user message rather than a real ask."""
  stripped = (content or "").lstrip()
  return any(stripped.startswith(prefix) for prefix in _AUTO_INJECTED_PREFIXES)


def _message_text(msg: dict) -> str:
  """Return the display text for a message, preferring full_content when present."""
  full_content = msg.get("full_content")
  if isinstance(full_content, str) and full_content:
    return full_content
  content = msg.get("content", "")
  return content if isinstance(content, str) else str(content)


def extract_recap(session_mgr: SessionManager, session_id: str, upto: int | None = None) -> dict:
  """Scan events [0, upto] and return ``{asks, last}`` via pure extraction (no LLM).

  asks: ordered first-lines of genuine user messages (auto-injected ones dropped),
        each truncated to ~80 chars.
  last: the final genuine user message + the assistant text that followed it,
        each truncated to ~250 chars; ``None`` if the session has no real asks.
  """
  count = session_mgr.get_chat_event_count_sync(session_id)
  end = count if upto is None else min(upto + 1, count)
  events, _ = session_mgr.load_chat_events_range(session_id, 0, end)
  messages = events_to_messages(events)

  asks: list[str] = []
  last_user_idx: int | None = None
  for i, msg in enumerate(messages):
    if msg.get("role") != "user" or _is_auto_injected(msg.get("content", "")):
      continue
    first = _first_line(msg.get("content", ""))
    if not first:
      continue
    asks.append(_truncate(first, _ASK_CHARS))
    last_user_idx = i

  last = None
  if last_user_idx is not None:
    assistant_text = ""
    for msg in messages[last_user_idx + 1:]:
      if msg.get("role") == "user":
        break
      if msg.get("role") == "assistant" and (msg.get("content") or "").strip():
        assistant_text = msg["content"]
    last = {
        "user": _truncate(messages[last_user_idx].get("content", ""), _LAST_CHARS),
        "assistant": _truncate(assistant_text, _LAST_CHARS),
    }

  return {"asks": asks, "last": last}


def _cache_path(session_mgr: SessionManager, session_id: str) -> Path:
  """Per-session recap-summary cache, sitting next to chat_events.jsonl."""
  return session_mgr.get_chat_events_path(session_id).parent / "recap_summaries.json"


def _load_cache(path: Path) -> dict:
  if not path.exists():
    return {}
  return json.loads(path.read_text(encoding="utf-8"))


def lookup_cached_summary(session_mgr: SessionManager, session_id: str, upto: int) -> tuple[str | None, bool]:
  """Return ``(summary, stale)`` for the divider at *upto*.

  Exact cache hit -> ``(summary, False)``. No exact hit but a summary computed at
  an earlier point exists -> ``(that_summary, True)`` since newer events are not
  yet reflected. Otherwise ``(None, False)``.
  """
  cache = _load_cache(_cache_path(session_mgr, session_id))
  exact = cache.get(str(upto))
  if exact is not None:
    return exact["summary"], False
  earlier = [int(k) for k in cache if int(k) < upto]
  if earlier:
    return cache[str(max(earlier))]["summary"], True
  return None, False


def _write_cache_entry(session_mgr: SessionManager, session_id: str, upto: int, summary: str) -> None:
  path = _cache_path(session_mgr, session_id)
  cache = _load_cache(path)
  cache[str(upto)] = {"summary": summary, "generated_at": utc_now().isoformat()}
  path.write_text(json.dumps(cache, ensure_ascii=False, indent=2), encoding="utf-8")


async def generate_and_cache_summary(
    session_mgr: SessionManager, session_id: str, upto: int, cfg: CharlieBotConfig) -> str:
  """Generate a recap summary for the divider at *upto*, cache it, and return it.

  Feeds the LLM ONLY the bounded extraction (asks + last), never raw events. The
  summary comes from a same-type light backend picked from model_preference. If the
  session backend is unknown or has no same-type preference entry, this logs a
  warning and returns "" without writing the cache (it never raises).
  """
  meta = await session_mgr.get_session(session_id)
  if meta is None or not meta.backend:
    log.warning("recap_skipped", reason="no_session_backend", session_id=session_id)
    return ""
  option = select_light_backend(cfg, meta.backend)
  if option is None:
    log.warning("recap_skipped", reason="no_same_type_preference", session_id=session_id, backend=meta.backend)
    return ""
  backend = build_backend(option, cfg)

  extract = await asyncio.to_thread(extract_recap, session_mgr, session_id, upto)
  asks = extract["asks"]
  last = extract["last"] or {}
  prompt = _SUMMARY_PROMPT.format(
      asks="\n".join(f"- {ask}" for ask in asks) if asks else "(none)",
      last_user=last.get("user") or "(none)",
      last_assistant=last.get("assistant") or "(none)",
  )
  summary = await backend.one_shot_text(prompt, _SUMMARY_SYSTEM_PROMPT, timeout=AUTONAMER_TIMEOUT)
  if summary:
    await asyncio.to_thread(_write_cache_entry, session_mgr, session_id, upto, summary)
    log.info("recap_summary_generated", session_id=session_id, upto=upto)
  return summary
