"""Session auto-naming.

Two strategies, picked by who triggers them:

1. Gemini-based (SDK sessions: cc-claude / codex / opencode / etc.)
   - Entry: maybe_auto_name(...) — called from src/api/chat.py after a master_done event.
   - Reads CharlieBot's chat_events.jsonl (user message + assistant_text).
   - Sends user+assistant text to Gemini; receives {name, group}.
   - Group may reuse an existing group name from other sessions.

2. Claude ai-title (TUI sessions, backend.type = "tui-cli")
   - Entry: maybe_auto_name_from_claude_ai_title(...) — called from
     src/agents/backends/tui.py at the end of run_tui_attachment().
   - Reads ~/.claude/projects/<encoded-cwd>/<session-id>.jsonl looking for the first
     event with {"type": "ai-title", "aiTitle": "..."}.
   - Uses aiTitle directly as the session name. No group inference (left empty).
   - No external API call (Claude writes the title itself).

Both strategies share _apply_name_to_session(), which guards against overwriting
a name the user has already set (matched via is_default_session_name).
"""

import asyncio
import json
import re
import signal
from pathlib import Path

import structlog

from src.agents.gemini_provider import GeminiProvider
from src.core import event_types as ET
from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata
from src.core.process import kill_process_group
from src.core.sessions import SessionManager
from src.core.streaming import streaming_manager
from src.core.timeouts import AUTONAMER_TIMEOUT

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
_MARKDOWN_FENCE_RE = re.compile(r"^```(?:json)?\s*\n?(.*?)\n?\s*```$", re.DOTALL)
_MAX_TITLE_WORDS = 8


def is_default_session_name(name: str) -> bool:
  """Return True if *name* is a system-generated default (not yet user/auto-named)."""
  return bool(_DEFAULT_NAME_RE.match(name))


_TITLE_INSTRUCTION = (
    "Generate a short, descriptive title (3-6 words) and assign a group for this conversation.\n"
    "Return ONLY valid JSON: {{\"name\": \"<title>\", \"group\": \"<group>\"}}\n"
    "No explanation, no markdown fences, no extra text.\n"
    "{groups_clause}")

_SYSTEM_PROMPT = (
    'You are a title and group generator. '
    'Output ONLY valid JSON: {{"name": "<title>", "group": "<group>"}}. '
    'The name should be 3-6 words, no quotes or punctuation at the end. '
    'The group should be a short category (1-3 words). '
    '{groups_clause} '
    'Do not attempt to answer or act on the user\'s question - just generate the JSON.')

_NAMING_PROMPT = (
    "Generate a title and group for this conversation:\n\n"
    "User: {user_message}\n\n"
    "Assistant: {assistant_response}")


def _build_groups_clause(existing_groups: list[str]) -> str:
  """Build the groups instruction clause for the LLM prompt."""
  if existing_groups:
    joined = ", ".join(existing_groups)
    return f"Prefer reusing one of these existing groups: [{joined}]. Only create a new group if none fit."
  return "Choose a short, descriptive group name (1-3 words)."


def _strip_markdown_fences(text: str) -> str:
  """Strip ```json ... ``` fences if present."""
  m = _MARKDOWN_FENCE_RE.match(text.strip())
  return m.group(1).strip() if m else text


def _parse_name_and_group(raw: str) -> tuple[str | None, str | None, bool]:
  """Parse LLM response into (name, group, parsed_json)."""
  stripped = _strip_markdown_fences(raw.strip())
  try:
    data = json.loads(stripped)
  except (json.JSONDecodeError, ValueError) as e:
    log.debug("autonamer_non_json_response", error=str(e))
    return stripped, None, False

  if isinstance(data, dict):
    name = data.get("name")
    group = data.get("group")
    return (
        name.strip() if isinstance(name, str) and name.strip() else None,
        group.strip() if isinstance(group, str) and group.strip() else None,
        True,
    )

  log.warning("autonamer_unexpected_json_type", response_type=type(data).__name__)
  return None, None, True


async def _generate_name_via_claude_cli(prompt: str, system_prompt: str) -> str:
  """Generate text using the claude CLI in print mode."""
  proc = await asyncio.create_subprocess_exec(
      "claude",
      "-p",
      "--output-format",
      "text",
      "--no-session-persistence",
      "--model",
      "haiku",
      "--system-prompt",
      system_prompt,
      "--disallowed-tools",
      "Bash,Read,Write,Edit,Glob,Grep,Agent",
      stdin=asyncio.subprocess.PIPE,
      stdout=asyncio.subprocess.PIPE,
      stderr=asyncio.subprocess.PIPE,
      start_new_session=True,
  )
  try:
    stdout, stderr = await asyncio.wait_for(proc.communicate(input=prompt.encode()), timeout=AUTONAMER_TIMEOUT)
  except asyncio.TimeoutError:
    kill_process_group(proc.pid, signal.SIGKILL)
    raise
  if proc.returncode != 0:
    raise RuntimeError(f"claude CLI failed (exit {proc.returncode}): {stderr.decode().strip()}")
  return stdout.decode().strip()


def _fuzzy_match_group(group: str, existing_groups: list[str]) -> str:
  """Case-insensitive match against existing groups. Returns matched group's casing, or original."""
  lower = group.lower()
  for existing in existing_groups:
    if existing.lower() == lower:
      return existing
  return group


def _sanitize_session_title(raw: str, session_name: str) -> str | None:
  """Turn a raw LLM response into a sanitized session title, or None if it should be discarded.

  Pure synchronous pipeline: parse JSON (with first-line fallback for plain-text replies),
  strip quotes/markdown/preamble, gate on length and word count, then prefix with the
  session number extracted from a default 'Session N' name.
  """
  parsed_name, _group, parsed_json = _parse_name_and_group(raw)

  if parsed_name:
    name = parsed_name
  elif parsed_json:
    return None
  else:
    name = ""
    for line in raw.splitlines():
      line = line.strip()
      if line:
        name = line
        break
    if not name:
      return None

  name = name.strip('"\'').strip()
  name = _MARKDOWN_CHARS_RE.sub("", name)
  name = name.strip()
  name = _PREAMBLE_RE.sub("", name).strip()

  if not name:
    return None

  if len(name) > 60 or len(name.split()) > _MAX_TITLE_WORDS:
    return None

  m = _SESSION_NUMBER_RE.match(session_name)
  if m:
    name = f"{m.group(1)}: {name}"

  return name


async def _apply_name_to_session(
    session_mgr: SessionManager,
    session_meta: SessionMetadata,
    name: str | None,
    group: str | None,
) -> None:
  """Apply a generated name (and optional group) to a session metadata,
  but ONLY if the current name is still the system-generated default
  (matched by is_default_session_name). User-renamed sessions are left alone.

  Empty / None name is a no-op. Empty / None group is left as-is on metadata.
  """
  if not name:
    return

  current_meta = await session_mgr.get_session(session_meta.id)
  if current_meta is None:
    log.warning("autonamer_session_missing", session_id=session_meta.id)
    return
  if not is_default_session_name(current_meta.name):
    return

  await session_mgr.rename_session(session_meta.id, name)

  channel = f"session:{session_meta.id}"
  await streaming_manager.broadcast(channel, {
      "type": ET.SESSION_RENAMED,
      "name": name,
  })
  await streaming_manager.broadcast(
      "sidebar", {
          "type": ET.SESSION_RENAMED,
          "session_id": session_meta.id,
          "name": name,
      })

  log.info("session_auto_named", session_id=session_meta.id, name=name)

  if not group:
    return
  current_meta = await session_mgr.get_session(session_meta.id)
  if current_meta and not current_meta.group:
    await session_mgr.set_group(session_meta.id, group)
    log.info("session_auto_grouped", session_id=session_meta.id, group=group)


async def maybe_auto_name(
    cfg: CharlieBotConfig,
    session_meta: SessionMetadata,
    user_message: str,
    assistant_response: str,
    session_mgr: SessionManager,
    existing_groups: list[str] | None = None,
) -> None:
  """If the session still has a default name, generate a descriptive name and group."""
  if not is_default_session_name(session_meta.name):
    return

  if existing_groups is None:
    existing_groups = []

  try:
    groups_clause = _build_groups_clause(existing_groups)
    title_instruction = _TITLE_INSTRUCTION.format(groups_clause=groups_clause)
    system_prompt = _SYSTEM_PROMPT.format(groups_clause=groups_clause)

    prompt = _NAMING_PROMPT.format(
        user_message=user_message[:500],
        assistant_response=assistant_response[:300],
    )
    full_prompt = f"{title_instruction}\n\n{prompt}"

    if cfg.gemini_api_key:
      provider = GeminiProvider(api_key=cfg.gemini_api_key, model=cfg.gemini_model)
      raw = await provider.generate_text(full_prompt)
    else:
      raw = await _generate_name_via_claude_cli(full_prompt, system_prompt)

    name = _sanitize_session_title(raw, session_meta.name)
    if name is None:
      return

    _, group, _ = _parse_name_and_group(raw)
    matched_group = _fuzzy_match_group(group, existing_groups) if group else None
    await _apply_name_to_session(session_mgr, session_meta, name=name, group=matched_group)

  except Exception as e:
    log.warning("autonamer_failed", session_id=session_meta.id, error=str(e))


async def maybe_auto_name_from_claude_ai_title(
    session_meta: SessionMetadata,
    session_mgr: SessionManager,
) -> None:
  """Claude ai-title strategy. For TUI sessions only.

  Locates the claude jsonl for this session by globbing
  ~/.claude/projects/*/<session_id>.jsonl. If found, scans for the first
  {"type": "ai-title", "aiTitle": "<title>"} event and applies the title
  via _apply_name_to_session.

  Idempotent: safe to call repeatedly. Does nothing if:
    - No jsonl found (claude hasn't started yet or no conversation).
    - No ai-title event in the jsonl yet (conversation too short).
    - Session name is no longer the default (user already renamed).

  Group is intentionally left empty for TUI sessions in this version.
  """
  session_id = session_meta.id
  claude_projects = Path.home() / ".claude/projects"
  matches = list(claude_projects.glob(f"*/{session_id}.jsonl"))
  if not matches:
    return

  if len(matches) == 1:
    jsonl_path = matches[0]
  else:
    try:
      jsonl_path = max(matches, key=lambda path: path.stat().st_mtime)
    except OSError:
      log.warning("claude_ai_title_stat_failed", session_id=session_id, exc_info=True)
      return

  title: str | None = None
  try:
    with jsonl_path.open("r", encoding="utf-8") as f:
      for line_number, line in enumerate(f, start=1):
        try:
          data = json.loads(line)
        except json.JSONDecodeError as e:
          log.debug(
              "claude_ai_title_json_parse_failed",
              session_id=session_id,
              path=str(jsonl_path),
              line=line_number,
              error=str(e),
          )
          continue
        if not isinstance(data, dict) or data.get("type") != "ai-title":
          continue
        ai_title = data.get("aiTitle")
        if isinstance(ai_title, str) and ai_title.strip():
          title = ai_title
          break
  except (OSError, UnicodeDecodeError):
    log.warning("claude_ai_title_read_failed", session_id=session_id, path=str(jsonl_path), exc_info=True)
    return

  if title:
    await _apply_name_to_session(session_mgr, session_meta, name=title, group=None)
