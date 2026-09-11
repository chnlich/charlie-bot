"""Verify-result trailer format — the single authority for the trailer regex and report reader.

This module owns the verify-trailer surface ``src.core.spawner`` needs to teach
verify workers the trailer format and to read a verify thread's final report. It knows nothing
about the plan registry; the dependency direction is spawner -> verify_trailer.
"""

import asyncio
import re
from pathlib import Path

from src.core import event_types as ET
from src.core.message_aggregator import extract_text_from_message
from src.core.ndjson import PARSE_SKIP_LOG_EVENT, iter_ndjson_events_from_end
from src.core.threads import ThreadManager

# ---------------------------------------------------------------------------
# Verify-result trailer — single authority
# ---------------------------------------------------------------------------

VERIFY_RESULT_TRAILER_RE = re.compile(r"RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \([0-9]+ approval\))")
VERIFY_RESULT_TRAILER_EXPECTED = f"`{VERIFY_RESULT_TRAILER_RE.pattern}`"


async def read_verify_final_report(session_id: str, thread_id: str, thread_mgr: ThreadManager) -> str:
  """Read the verifier's complete final result, falling back to its last assistant text.

  The report is the log's last ``result`` event's payload when it carries
  non-empty text; every other shape (no result event, an empty payload) falls
  back to the last assistant event with non-empty text, and a log with neither
  reads as empty. Both judgments are one from-the-end walk
  (:func:`src.core.ndjson.iter_ndjson_events_from_end`) resolved in one thread
  hop, so a verify finalize reads only the trailing bytes its answer needs —
  the RESULT event sits at the log tail — instead of parsing the whole file.
  """
  events_path = await thread_mgr.get_events_log_path(session_id, thread_id)
  return await asyncio.to_thread(_resolve_final_report, events_path)


def _resolve_final_report(events_path: Path) -> str:
  """One from-the-end pass deciding the report: the last result event's payload, else the last assistant text.

  The result judgment settles at the first result event from the end — a usable
  payload returns, an empty one hands the answer to the assistant fallback, the
  whole-list walk's first-hit-then-break rule. The assistant judgment settles at
  the first non-empty assistant text from the end. The walk returns once both
  judgments have settled and keeps walking only while one of them is still open
  (no result event seen yet, or the fallback text not yet found).
  """
  assistant_text: str | None = None
  seen_result = False
  for event in iter_ndjson_events_from_end(events_path, log_event=PARSE_SKIP_LOG_EVENT, log_fields={}):
    event_type = event.get("type")
    if assistant_text is None and event_type == ET.ASSISTANT:
      message = event.get("message") if isinstance(event.get("message"), dict) else None
      text = extract_text_from_message(message)
      if text.strip():
        assistant_text = text
    if not seen_result and event_type == ET.RESULT:
      seen_result = True
      result = event.get("result")
      if isinstance(result, str) and result.strip():
        return result
    elif seen_result and assistant_text is not None:
      # Both judgments settled: the empty result handed the answer to the
      # fallback, and the fallback text is older-ward from here.
      return assistant_text
  return assistant_text or ""


def _normalize_line(line: str) -> str:
  """Strip whitespace and repeated markdown wrappers (backticks, asterisks) to a fixed point.

  Dashes are left in place; Slack marker matching depends on them surviving.
  """
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
