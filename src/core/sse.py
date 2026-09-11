"""Spec-conformant SSE line framing shared by SSE-consuming call sites.

httpx's ``aiter_lines()`` splits on Python ``str.splitlines`` semantics, which
treats U+0085/U+2028/U+2029 (among others) as line boundaries. SSE frames
legitimately carry those characters inside JSON string values, so frames get
cut mid-string and ``json.loads`` fails with "Unterminated string". This module
frames lines on the SSE terminators only: CRLF, LF, CR.
"""

import re
from collections.abc import AsyncIterator
from typing import Any

_TERMINATOR_RE = re.compile(r"\r\n|\r|\n")


def split_sse_lines(buffer: str, *, final: bool, scanned_to: int = 0) -> tuple[list[str], str]:
  """Split *buffer* on the SSE terminators {CRLF, LF, CR} into (lines, remainder).

  A buffer ending in ``\\r`` is held back when ``final`` is false, because the
  CR may pair with a leading LF in the next chunk. With ``final=True`` a
  trailing unterminated line is flushed as a line and the remainder is empty.

  ``scanned_to`` resumes the terminator search at that index; emitted lines
  still span from the buffer start. The caller's contract: no terminator
  begins before ``scanned_to``. A streaming caller establishes it from the
  previous call's remainder, which is terminator-free except for a held-back
  trailing CR, and that CR belongs to the cursor so it can pair with the next
  chunk's leading LF. Without resumption every chunk re-searches the whole
  accumulated remainder and framing a multi-chunk frame costs O(bytes x
  chunks) instead of O(bytes) — seconds of event-loop time per multi-MB
  SSE payload at network chunk sizes.
  """
  end = len(buffer)
  if not final and end and buffer[end - 1] == "\r":
    end -= 1
  lines: list[str] = []
  start = 0
  for match in _TERMINATOR_RE.finditer(buffer, scanned_to, end):
    lines.append(buffer[start:match.start()])
    start = match.end()
  if final:
    if start < len(buffer):
      lines.append(buffer[start:])
    return lines, ""
  return lines, buffer[start:]


class _ChunkedFramer:
  """Incremental SSE framing over byte chunks with O(bytes) total work.

  ``pieces`` carries the current partial line as a fragment list; its
  concatenation holds no resolved terminator and may end with one held-back
  CR. Each search covers the new chunk only and runs at bytes.find's memchr
  speed — the alternation regex pays the re engine instead, 0.087 s per 16 MB
  measured on this host. Each emitted line is joined once; concatenating the
  accumulated remainder per chunk costs O(bytes x chunks per frame). Framing
  semantics (terminator set, held-back trailing CR, final flush) follow
  :func:`split_sse_lines`. The terminators are ASCII, so byte-level framing
  can never split a UTF-8 multibyte character: every continuation byte sits
  at 0x80 or above.
  """

  def __init__(self) -> None:
    self.pieces: list[bytes] = []

  def feed(self, text: bytes, *, final: bool) -> list[bytes]:
    """Consume one byte chunk and return the lines it completes.

    Callers pass ``final=False`` only with non-empty *text*: an empty chunk
    carries no new data, so it must never resolve a held-back CR. With
    ``final=True`` a trailing CR counts as a full terminator and the
    unterminated tail stays in ``pieces`` for the caller's closing join.
    """
    lines: list[bytes] = []
    start = 0
    if self.pieces and self.pieces[-1].endswith(b"\r"):
      # The CR held back from the previous chunk resolves on the first
      # byte here: a leading LF pairs with it, anything else leaves it a
      # lone CR terminator. Either way the line ends at its start.
      self.pieces[-1] = self.pieces[-1][:-1]
      lines.append(b"".join(self.pieces))
      self.pieces.clear()
      if text.startswith(b"\n"):
        start = 1
    end = len(text)
    if not final and end > start and text[end - 1] == 0x0D:
      end -= 1
    # The next CR's position is cached across emitted lines: a per-line CR
    # find would re-scan the whole chunk remainder per line on CR-free
    # streams — O(bytes x lines) instead of O(bytes).
    cr = text.find(b"\r", start, end)
    while True:
      i_lf = text.find(b"\n", start, end)
      if i_lf == -1:
        if cr == -1:
          break
        term_at, step = cr, 1
      elif cr == -1 or i_lf < cr:
        term_at, step = i_lf, 1
      else:
        term_at = cr
        step = 2 if cr + 1 < len(text) and text[cr + 1] == 0x0A else 1
      if self.pieces:
        # The tail joins with the pieces instead of concatenating onto the
        # join's fresh result: a concat on a just-allocated large string
        # re-allocates through the mmap lane and re-faults the line's pages
        # (~0.9 ms per 1 MB line measured vs ~0.03 ms for one join of the
        # same pieces), which once per multi-hundred-KB frame is the
        # framer's dominant cost.
        self.pieces.append(text[start:term_at])
        lines.append(b"".join(self.pieces))
        self.pieces.clear()
      else:
        lines.append(text[start:term_at])
      start = term_at + step
      if cr != -1 and cr < start:
        cr = text.find(b"\r", start, end)
    if start < len(text):
      self.pieces.append(text[start:])
    return lines


async def iter_sse_lines(response: Any, *, lines_as_bytes: bool = False) -> AsyncIterator[str | bytes]:
  """Yield SSE lines from ``response.aiter_bytes()`` framed on CRLF/LF/CR only.

  The default mode decodes each completed line as UTF-8 with
  ``errors="replace"`` (parity with httpx's TextDecoder), so a multibyte
  character split across byte chunks survives and invalid bytes degrade to
  U+FFFD. ``lines_as_bytes=True`` yields the wire's raw bytes instead: the
  framing never splits a multibyte character (the terminators are ASCII), so
  the consumer's JSON parse sees the reassembled characters — and an invalid
  byte raises at that parse instead of degrading. The per-chunk UTF-8 decode
  the default mode pays is the framer's largest cost (8.7 ms per 16 MB
  measured), which the byte mode exists to skip on the JSON-parse consumers.
  """
  framer = _ChunkedFramer()
  async for chunk in response.aiter_bytes():
    if not chunk:
      continue
    for line in framer.feed(chunk, final=False):
      yield line if lines_as_bytes else line.decode("utf-8", errors="replace")
  for line in framer.feed(b"", final=True):
    yield line if lines_as_bytes else line.decode("utf-8", errors="replace")
  if framer.pieces:
    tail = b"".join(framer.pieces)
    yield tail if lines_as_bytes else tail.decode("utf-8", errors="replace")
