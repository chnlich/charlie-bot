"""Property tests for the spec-conformant SSE line splitter (src/core/sse.py)."""

import pytest
from conftest import FakeChunkedResponse

from src.core.sse import iter_sse_lines, split_sse_lines


async def _drain_lines(chunks: list[bytes]) -> list[str]:
  return [line async for line in iter_sse_lines(FakeChunkedResponse(chunks))]


def test_trailing_cr_is_held_until_next_chunk_or_final() -> None:
  assert split_sse_lines("data: x\r", final=False) == ([], "data: x\r")
  assert split_sse_lines("data: x\r" + "\n", final=False) == (["data: x"], "")


def test_crlf_straddling_two_chunks_yields_one_line() -> None:
  lines, held = split_sse_lines("data: x\r", final=False)
  assert not lines
  assert held == "data: x\r"
  lines, remainder = split_sse_lines(held + "\ndata: y\n", final=False)
  assert lines == ["data: x", "data: y"]
  assert remainder == ""


def test_final_flush_emits_unterminated_tail_line() -> None:
  lines, remainder = split_sse_lines("data: tail", final=True)
  assert lines == ["data: tail"]
  assert remainder == ""


@pytest.mark.asyncio
async def test_adapter_decodes_multibyte_character_split_across_chunks() -> None:
  line = "data: caf\u00e9"
  wire = (line + "\n").encode("utf-8")
  cut = wire.index(b"\xc3") + 1
  assert await _drain_lines([wire[:cut], wire[cut:]]) == [line]


@pytest.mark.asyncio
async def test_large_frame_spanning_many_chunks_frames_like_one_buffer() -> None:
  # The resumable-scan worst case in production: multi-chunk frames with the
  # terminator only at the frame end.
  frame = "data: " + "x" * 100_000
  raw = (frame + "\r\n\r\n" + frame + "\n\n").encode()
  chunks = [raw[i:i + 4096] for i in range(0, len(raw), 4096)]
  assert await _drain_lines(chunks) == [frame, "", frame, ""]


# ---------------------------------------------------------------------------
# Byte mode (lines_as_bytes=True) — the production SSE consumers' shape.
# ---------------------------------------------------------------------------


async def _drain_lines_bytes(chunks: list[bytes]) -> list[bytes]:
  return [line async for line in iter_sse_lines(FakeChunkedResponse(chunks), lines_as_bytes=True)]


@pytest.mark.asyncio
async def test_byte_mode_matches_str_mode_on_every_two_way_split() -> None:
  stream = "data: a\r\ndata: b\ndata: c\r\r\n\n"
  wire = stream.encode()
  for cut in range(len(wire) + 1):
    chunks = [wire[:cut], wire[cut:]]
    assert await _drain_lines_bytes(chunks) == [line.encode() for line in await _drain_lines(chunks)], cut
