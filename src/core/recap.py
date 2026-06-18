"""In-session recap: cheap pure-extraction plus an opt-in cached Haiku summary.

The default path is zero-token. ``extract_recap`` scans a session's (sparse)
user events and returns the ordered asks plus the last exchange — no LLM call.
A concise Haiku summary is generated only on an explicit request and cached per
``(session_id, upto)`` so reopening an unchanged divider costs nothing.
"""

import asyncio
import json
from pathlib import Path

import structlog

from src.api.message_utils import events_to_messages
from src.core.autonamer import _generate_name_via_claude_cli
from src.core.models import utc_now
from src.core.sessions import SessionManager

log = structlog.get_logger()

_ASK_CHARS = 80
_LAST_CHARS = 250

# User messages auto-injected by the system are not real "asks". Matched by prefix
# against the trigger banner and the fork/elone bootstrap prompts.
_AUTO_INJECTED_PREFIXES = (
    "[Scheduled trigger fired",
    "This session was cloned from a previous conversation.",
    "You're taking over a task from a previous session where user wasn't satisfied.",
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

_CONDENSED_CHARS = 1000
RECAP_CONTEXT_BUDGET_BYTES = 64 * 1024


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


def _split_rounds(messages: list[dict]) -> list[list[dict]]:
  """Group messages into rounds opened by genuine user asks."""
  rounds: list[list[dict]] = []
  current: list[dict] | None = None
  for msg in messages:
    role = msg.get("role")
    text = _message_text(msg)
    if role == "user" and not _is_auto_injected(text):
      if current is not None:
        rounds.append(current)
      current = [msg]
    elif current is not None:
      current.append(msg)
  if current is not None:
    rounds.append(current)
  return rounds


def _format_tool_value(value) -> str:
  if isinstance(value, (dict, list)):
    return json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True)
  return "" if value is None else str(value)


def _truncate_to_cap(text: str, limit: int) -> str:
  text = (text or "").strip()
  return text if len(text) <= limit else _truncate(text, limit - 1)


def _utf8_len(text: str) -> int:
  return len(text.encode("utf-8"))


def _truncate_to_bytes(text: str, limit: int) -> str:
  if limit <= 0:
    return ""
  encoded = text.encode("utf-8")
  if len(encoded) <= limit:
    return text
  suffix = "…"
  suffix_bytes = suffix.encode("utf-8")
  if limit <= len(suffix_bytes):
    return encoded[:limit].decode("utf-8", errors="ignore")
  clipped = encoded[:limit - len(suffix_bytes)].decode("utf-8", errors="ignore").rstrip()
  return clipped + suffix


def _join_round_lines(lines: list[str]) -> str:
  return "\n".join(lines)


def _render_condensed_round(round_messages: list[dict], round_number: int) -> list[str]:
  lines = [f"### Round {round_number} (condensed)"]
  user_text = _message_text(round_messages[0])
  if user_text:
    lines.append(f"User: {_truncate(user_text, _CONDENSED_CHARS)}")

  assistant_text = ""
  conclusions: list[tuple[str, str]] = []
  for msg in round_messages[1:]:
    role = msg.get("role")
    text = _message_text(msg)
    if role == "assistant" and text.strip():
      assistant_text = text
    elif role == "worker_summary" and text.strip():
      conclusions.append(("Worker summary", text))
    elif role == "task_delegated" and text.strip():
      conclusions.append(("Task delegated", text))

  if assistant_text:
    lines.append(f"Assistant: {_truncate(assistant_text, _CONDENSED_CHARS)}")
  for label, text in conclusions:
    clipped = _truncate(text, _CONDENSED_CHARS)
    if label == "Task delegated" and clipped.startswith("Task delegated:"):
      lines.append(clipped)
    else:
      lines.append(f"{label}: {clipped}")
  return lines


def _render_full_message(msg: dict, tool_output_cap: int) -> list[str]:
  role = msg.get("role")
  text = _message_text(msg)
  if role == "user":
    if _is_auto_injected(text):
      return []
    return ["**User**", text]
  if role == "assistant":
    lines: list[str] = []
    if text.strip():
      lines.extend(["**Assistant**", text])
    for tool in msg.get("tools") or []:
      name = tool.get("name", "")
      status = " error" if tool.get("is_error") else ""
      lines.append(f"**Tool: {name}{status}**")
      lines.append("Input:")
      lines.append("```json")
      lines.append(_format_tool_value(tool.get("input", {})))
      lines.append("```")
      lines.append("Output:")
      lines.append("```text")
      lines.append(_truncate_to_cap(str(tool.get("output", "")), tool_output_cap))
      lines.append("```")
    return lines
  if role == "worker_summary" and text.strip():
    return ["**Worker summary**", text]
  if role == "task_delegated" and text.strip():
    return ["**Task delegated**", text]
  if role == "system" and text.strip():
    return ["**System**", text]
  return []


def _render_full_round(round_messages: list[dict], round_number: int, tool_output_cap: int) -> list[str]:
  lines = [f"### Round {round_number} (full)"]
  for msg in round_messages:
    rendered = _render_full_message(msg, tool_output_cap)
    if rendered:
      lines.extend(rendered)
  return lines


def build_recap_context(
    session_mgr: SessionManager,
    session_id: str,
    upto: int | None = None,
    full_rounds: int = 2,
    tool_output_cap: int = 4000,
    total_budget_bytes: int = RECAP_CONTEXT_BUDGET_BYTES,
) -> str:
  """Build a deterministic two-tier recent-context transcript for bootstraps."""
  count = session_mgr.get_chat_event_count_sync(session_id)
  end = count if upto is None else min(upto + 1, count)
  events, _ = session_mgr.load_chat_events_range(session_id, 0, end)
  messages = events_to_messages(events)
  rounds = _split_rounds(messages)

  if not rounds:
    return "No genuine user turns were found before the takeover point."

  full_start = max(len(rounds) - full_rounds, 0) if full_rounds > 0 else len(rounds)
  header = _join_round_lines([
      "# Reconstructed Recent Context",
      "",
      "Newest turns are shown first. Recent turns are shown in full when they fit; older turns are condensed.",
  ])
  parts: list[str] = []
  used_bytes = 0

  def append_block(block: str, *, truncate: bool = False) -> bool:
    nonlocal used_bytes
    separator = "\n\n" if parts else ""
    remaining = total_budget_bytes - used_bytes - _utf8_len(separator)
    if remaining <= 0:
      return False
    block_bytes = _utf8_len(block)
    if block_bytes > remaining:
      if not truncate:
        return False
      block = _truncate_to_bytes(block, remaining)
      if not block:
        return False
      block_bytes = _utf8_len(block)
    parts.append(separator + block)
    used_bytes += _utf8_len(separator) + block_bytes
    return True

  append_block(header, truncate=True)
  for round_index in range(len(rounds) - 1, -1, -1):
    round_number = round_index + 1
    round_messages = rounds[round_index]
    prefer_full = round_index >= full_start
    if prefer_full:
      full_block = _join_round_lines(_render_full_round(round_messages, round_number, tool_output_cap))
      if append_block(full_block, truncate=round_index == len(rounds) - 1):
        continue
      condensed_block = _join_round_lines(_render_condensed_round(round_messages, round_number))
      if not append_block(condensed_block):
        break
      continue

    condensed_block = _join_round_lines(_render_condensed_round(round_messages, round_number))
    if not append_block(condensed_block):
      break
  return "".join(parts)


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


async def generate_and_cache_summary(session_mgr: SessionManager, session_id: str, upto: int) -> str:
  """Generate a Haiku recap for the divider at *upto*, cache it, and return it.

  Feeds the LLM ONLY the bounded extraction (asks + last), never raw events.
  """
  extract = await asyncio.to_thread(extract_recap, session_mgr, session_id, upto)
  asks = extract["asks"]
  last = extract["last"] or {}
  prompt = _SUMMARY_PROMPT.format(
      asks="\n".join(f"- {ask}" for ask in asks) if asks else "(none)",
      last_user=last.get("user") or "(none)",
      last_assistant=last.get("assistant") or "(none)",
  )
  summary = await _generate_name_via_claude_cli(prompt, _SUMMARY_SYSTEM_PROMPT)
  await asyncio.to_thread(_write_cache_entry, session_mgr, session_id, upto, summary)
  log.info("recap_summary_generated", session_id=session_id, upto=upto)
  return summary
