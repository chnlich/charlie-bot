"""Verify-result trailer format — the single authority for the trailer regex and report reader.

This module owns the verify-trailer surface that ``src.core.spawner`` still needs to teach
verify workers the trailer format and to read a verify thread's final report. It knows nothing
about the plan registry; the dependency direction is spawner -> verify_trailer, and plans.py
no longer references any of these symbols.
"""

import asyncio
import re

from src.api.message_utils import extract_text_from_message
from src.core import event_types as ET
from src.core.ndjson import parse_ndjson_file
from src.core.threads import ThreadManager

# ---------------------------------------------------------------------------
# Verify-result trailer — single authority
# ---------------------------------------------------------------------------

VERIFY_RESULT_TRAILER_RE = re.compile(r"RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \([0-9]+ approval\))")
VERIFY_RESULT_TRAILER_EXPECTED = f"`{VERIFY_RESULT_TRAILER_RE.pattern}`"


async def read_verify_final_report(session_id: str, thread_id: str, thread_mgr: ThreadManager) -> str:
  """Read the verifier's complete final result, falling back to its last assistant text."""
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  events = await asyncio.to_thread(parse_ndjson_file, events_path)

  for ev in reversed(events):
    if ev.get("type") != ET.RESULT:
      continue
    result = ev.get("result")
    if isinstance(result, str) and result.strip():
      return result
    break

  for ev in reversed(events):
    if ev.get("type") != ET.ASSISTANT:
      continue
    message = ev.get("message") if isinstance(ev.get("message"), dict) else None
    text = extract_text_from_message(message)
    if text.strip():
      return text
  return ""


def _normalize_line(line: str) -> str:
  """Strip whitespace and repeated markdown wrappers (backticks, asterisks) to a fixed point."""
  normalized = line.strip()
  while True:
    stripped = normalized.strip("`*").strip()
    if stripped == normalized:
      return normalized
    normalized = stripped


def verify_result_trailer_error(report: str) -> str:
  """Return an explicit verifier completion error, or an empty string for a valid trailer.

  A report is valid when any line, scanning from the end toward the start, normalizes to a
  RESULT trailer line matching the regex. The last such line is taken as the final verdict,
  so a valid trailer remains accepted even when it is markdown-wrapped or followed by prose.
  """
  expected = VERIFY_RESULT_TRAILER_EXPECTED
  if not report.strip():
    return f"Verifier final report is empty; expected a final {expected} line."
  for raw_line in reversed(report.splitlines()):
    if VERIFY_RESULT_TRAILER_RE.fullmatch(_normalize_line(raw_line)):
      return ""
  return f"Verifier final report has a missing or malformed `RESULT:` trailer; expected a final {expected} line."
