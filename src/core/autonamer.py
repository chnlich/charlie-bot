"""Auto-name and auto-group sessions after the first chat turn using Gemini or Claude CLI."""

import asyncio
import json
import re
import signal

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

    # Parse JSON response or fall back to plain-text name
    parsed_name, group, parsed_json = _parse_name_and_group(raw)

    # Use parsed name or fall back to raw first-line extraction for plain-text responses.
    if parsed_name:
      name = parsed_name
    elif parsed_json:
      return
    else:
      name = ""
      for line in raw.splitlines():
        line = line.strip()
        if line:
          name = line
          break
      if not name:
        return

    # Sanitize: extract a concise title from potentially verbose LLM output
    # 1. Strip quotes and markdown formatting
    name = name.strip('"\'').strip()
    name = _MARKDOWN_CHARS_RE.sub("", name)
    name = name.strip()

    # 2. Strip common LLM preamble patterns
    name = _PREAMBLE_RE.sub("", name).strip()

    if not name:
      return

    # 3. Discard if still too long or looks like a sentence
    if len(name) > 60 or len(name.split()) > _MAX_TITLE_WORDS:
      return

    # Prefix with session number extracted from 'Session N'
    m = _SESSION_NUMBER_RE.match(session_meta.name)
    if m:
      name = f"{m.group(1)}: {name}"

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

    # Auto-assign group if LLM provided one and session doesn't already have a group
    current_meta = await session_mgr.get_session(session_meta.id)
    if group and current_meta and not current_meta.group:
      matched_group = _fuzzy_match_group(group, existing_groups)
      await session_mgr.set_group(session_meta.id, matched_group)
      log.info("session_auto_grouped", session_id=session_meta.id, group=matched_group)

  except Exception as e:
    log.warning("autonamer_failed", session_id=session_meta.id, error=str(e))
