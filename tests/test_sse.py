"""Tests for src.core.sse: spec-conformant SSE line framing."""

import random
import re

import pytest

from src.core.sse import iter_sse_lines, split_sse_lines

_TERMINATOR_RE = re.compile(r"\r\n|\n|\r")

# The full str.splitlines boundary set (every member is < U+3000).
_SPLITLINES_BOUNDARIES = {
    "\n",
    "\r",
    "\x0b",
    "\x0c",
    "\x1c",
    "\x1d",
    "\x1e",
    "\x85",
    "\u2028",
    "\u2029",
}


def _reference_lines(text: str) -> list[str]:
  """Independent reference: split on {CRLF, LF, CR}; a trailing terminator
  leaves no extra empty segment (str.splitlines semantics)."""
  if not text:
    return []
  segments = _TERMINATOR_RE.split(text)
  if _TERMINATOR_RE.fullmatch(text[-1]):
    segments.pop()
  return segments


def _derive_splitlines_boundaries() -> set[str]:
  """Exhaustively derive the boundary set from str.splitlines over range(0x3000)."""
  return {
      chr(cp) for cp in range(0x3000)
      if ("a" + chr(cp) + "b").splitlines() != ["a" + chr(cp) + "b"]
  }


def _feed_pieces(pieces: list[str]) -> list[str]:
  buffer = ""
  lines: list[str] = []
  for piece in pieces[:-1]:
    emitted, buffer = split_sse_lines(buffer + piece, final=False)
    lines.extend(emitted)
  emitted, buffer = split_sse_lines(buffer + pieces[-1], final=True)
  lines.extend(emitted)
  return lines


def _all_chunkings(text: str) -> list[list[str]]:
  chunkings: list[list[str]] = []
  for mask in range(1 << (len(text) - 1)):
    pieces: list[str] = []
    current = text[0]
    for i in range(1, len(text)):
      if mask & (1 << (i - 1)):
        pieces.append(current)
        current = text[i]
      else:
        current += text[i]
    pieces.append(current)
    chunkings.append(pieces)
  return chunkings


class _FakeBytesResponse:
  """httpx.Response double exposing only aiter_bytes() over pre-chunked bytes."""

  def __init__(self, chunks: list[bytes]) -> None:
    self._chunks = chunks

  async def aiter_bytes(self):
    for chunk in self._chunks:
      yield chunk


def test_derived_splitlines_boundary_set_is_complete() -> None:
  assert _derive_splitlines_boundaries() == _SPLITLINES_BOUNDARIES


def test_terminator_forms_each_yield_one_line_without_residue() -> None:
  texts = ["a\nb\nc", "a\r\nb\r\nc", "a\rb\rc", "a\r\nb\nc\rd\r\n"]
  for text in texts:
    lines, remainder = split_sse_lines(text, final=True)
    assert lines == _reference_lines(text)
    assert lines == text.splitlines()  # CR/LF-only text agrees with splitlines
    assert remainder == ""
    for line in lines:
      assert "\n" not in line
      assert "\r" not in line


def test_empty_and_terminator_only_buffers() -> None:
  assert split_sse_lines("", final=False) == ([], "")
  assert split_sse_lines("", final=True) == ([], "")
  assert split_sse_lines("\n\n", final=True) == (["", ""], "")
  assert split_sse_lines("\r\n", final=True) == ([""], "")
  assert split_sse_lines("\r", final=True) == ([""], "")
  assert split_sse_lines("\r", final=False) == ([], "\r")


def test_trailing_cr_holds_until_next_chunk_resolves_crlf_vs_bare_cr() -> None:
  # \r\n straddling the chunk boundary: one line, no spurious empty line.
  _lines, remainder = split_sse_lines("abc\r", final=False)
  assert remainder == "abc\r"
  lines, remainder = split_sse_lines(remainder + "\ndef\n", final=True)
  assert lines == ["abc", "def"]
  assert remainder == ""
  # bare \r straddling the chunk boundary: still exactly two lines.
  _lines, remainder = split_sse_lines("abc\r", final=False)
  lines, remainder = split_sse_lines(remainder + "def\n", final=True)
  assert lines == ["abc", "def"]
  assert remainder == ""


def test_final_flush_emits_trailing_unterminated_line() -> None:
  lines, remainder = split_sse_lines("data: x\npart", final=False)
  assert lines == ["data: x"]
  assert remainder == "part"
  lines, remainder = split_sse_lines(remainder + "ial", final=True)
  assert lines == ["partial"]
  assert remainder == ""


def test_trailing_cr_at_stream_end_flushes_bare_cr_terminated_line() -> None:
  lines, remainder = split_sse_lines("abc\r", final=True)
  assert lines == ["abc"]
  assert remainder == ""


def test_boundary_characters_other_than_cr_lf_stay_inside_lines() -> None:
  for char in sorted(_derive_splitlines_boundaries() - {"\r", "\n"}):
    text = "pre" + char + "post\n"
    lines, remainder = split_sse_lines(text, final=True)
    assert lines == ["pre" + char + "post"], f"U+{ord(char):04X} must not terminate a line"
    assert remainder == ""


def test_exhaustive_chunkings_match_single_shot_reference() -> None:
  text = "a\r\nb\nc\rd\x85e\u2028f\u2029\n"
  expected = _reference_lines(text)
  assert "\x85" in text
  assert "\u2028" in text
  assert "\u2029" in text
  for pieces in _all_chunkings(text):
    assert _feed_pieces(pieces) == expected, f"chunking failed: {pieces}"


@pytest.mark.asyncio
async def test_iter_sse_lines_byte_chunkings_including_mid_multibyte() -> None:
  text = "data: h\x85e\u2028l\u2029lo\r\nwor\rld\npart"
  expected = _reference_lines(text)
  data = text.encode("utf-8")
  for cut in range(1, len(data)):
    lines = [line async for line in iter_sse_lines(_FakeBytesResponse([data[:cut], data[cut:]]))]
    assert lines == expected, f"single cut at byte {cut} changed the lines"
  rng = random.Random(20260828)
  for _ in range(50):
    cuts = sorted(rng.sample(range(1, len(data)), k=rng.randint(2, min(8, len(data) - 1))))
    chunks: list[bytes] = []
    previous = 0
    for cut in cuts:
      chunks.append(data[previous:cut])
      previous = cut
    chunks.append(data[previous:])
    lines = [line async for line in iter_sse_lines(_FakeBytesResponse(chunks))]
    assert lines == expected, f"chunking failed: {chunks}"


@pytest.mark.asyncio
async def test_iter_sse_lines_replaces_invalid_utf8_like_httpx_text_decoder() -> None:
  lines = [line async for line in iter_sse_lines(_FakeBytesResponse([b"data: {\xff}\n\n"]))]
  assert lines == ["data: {\ufffd}", ""]


@pytest.mark.asyncio
async def test_iter_sse_lines_empty_stream_yields_nothing() -> None:
  lines = [line async for line in iter_sse_lines(_FakeBytesResponse([]))]
  assert lines == []
