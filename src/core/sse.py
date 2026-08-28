"""Spec-conformant SSE line framing shared by SSE consumers.

The WHATWG Server-Sent Events spec terminates event-stream lines with CR, LF,
or CRLF only. httpx's ``aiter_lines()`` instead applies Python ``str.splitlines``
semantics, which additionally splits on U+0085, U+2028, U+2029, VT, FF, and the
file/group/record separators. An SSE frame whose JSON payload legitimately
carries such a character inside a string is cut mid-string; the continuation
line carries no ``data:`` prefix and is dropped, so ``json.loads`` fails with
``Unterminated string`` and kills the run. This module owns SSE line framing in
one place; consumers keep their per-line parsing logic unchanged.
"""

import codecs
from collections.abc import AsyncIterator

import httpx


def split_sse_lines(buffer: str, *, final: bool) -> tuple[list[str], str]:
  """Split complete SSE lines off ``buffer`` and return ``(lines, remainder)``.

  Line terminators are exactly CRLF, LF, and bare CR; every other character
  (including the remaining ``str.splitlines`` boundaries) is line content. When
  ``buffer`` ends with ``\\r`` and ``final`` is false, the unterminated tail is
  held back because a CRLF pair may straddle chunks; with ``final`` true the
  ``\\r`` is a bare terminator and any trailing unterminated line is flushed.
  Pure function: no I/O, no imports beyond the stdlib.
  """
  lines: list[str] = []
  start = 0
  i = 0
  n = len(buffer)
  while i < n:
    char = buffer[i]
    if char == "\n":
      lines.append(buffer[start:i])
      i += 1
      start = i
    elif char == "\r":
      if i + 1 == n and not final:
        break
      lines.append(buffer[start:i])
      i += 2 if i + 1 < n and buffer[i + 1] == "\n" else 1
      start = i
    else:
      i += 1
  remainder = buffer[start:]
  if final and remainder:
    lines.append(remainder)
    remainder = ""
  return lines, remainder


async def iter_sse_lines(response: httpx.Response) -> AsyncIterator[str]:
  """Yield SSE lines from ``response.aiter_bytes()``.

  Decodes incrementally with a UTF-8 decoder using ``errors="replace"``
  (parity with httpx's ``TextDecoder``), splits each decoded piece with
  ``split_sse_lines`` (spec terminators only), and flushes the trailing
  unterminated line at stream end.
  """
  decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
  buffer = ""
  async for chunk in response.aiter_bytes():
    lines, buffer = split_sse_lines(buffer + decoder.decode(chunk), final=False)
    for line in lines:
      yield line
  lines, _ = split_sse_lines(buffer + decoder.decode(b"", True), final=True)
  for line in lines:
    yield line
