"""Spec-conformant SSE line framing shared by SSE-consuming call sites.

httpx's ``aiter_lines()`` splits on Python ``str.splitlines`` semantics, which
treats U+0085/U+2028/U+2029 (among others) as line boundaries. SSE frames
legitimately carry those characters inside JSON string values, so frames get
cut mid-string and ``json.loads`` fails with "Unterminated string". This module
frames lines on the SSE terminators only: CRLF, LF, CR.
"""

import codecs
import re
from collections.abc import AsyncIterator
from typing import Any

_TERMINATOR_RE = re.compile(r"\r\n|\r|\n")


def split_sse_lines(buffer: str, *, final: bool) -> tuple[list[str], str]:
  """Split *buffer* on the SSE terminators {CRLF, LF, CR} into (lines, remainder).

  A buffer ending in ``\\r`` is held back when ``final`` is false, because the
  CR may pair with a leading LF in the next chunk. With ``final=True`` a
  trailing unterminated line is flushed as a line and the remainder is empty.
  """
  end = len(buffer)
  if not final and end and buffer[end - 1] == "\r":
    end -= 1
  lines: list[str] = []
  start = 0
  for match in _TERMINATOR_RE.finditer(buffer, 0, end):
    lines.append(buffer[start:match.start()])
    start = match.end()
  if final:
    if start < len(buffer):
      lines.append(buffer[start:])
    return lines, ""
  return lines, buffer[start:]


async def iter_sse_lines(response: Any) -> AsyncIterator[str]:
  """Yield SSE lines from ``response.aiter_bytes()`` framed on CRLF/LF/CR only.

  Decodes incrementally as UTF-8 with ``errors="replace"`` (parity with httpx's
  TextDecoder), so a multibyte character split across byte chunks survives.
  """
  decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
  buffer = ""
  async for chunk in response.aiter_bytes():
    buffer += decoder.decode(chunk)
    lines, buffer = split_sse_lines(buffer, final=False)
    for line in lines:
      yield line
  buffer += decoder.decode(b"", final=True)
  lines, _ = split_sse_lines(buffer, final=True)
  for line in lines:
    yield line
