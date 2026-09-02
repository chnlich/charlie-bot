"""Property tests for the spec-conformant SSE line splitter (src/core/sse.py)."""

import json
import random

import pytest
from conftest import FakeChunkedResponse

from src.core.sse import iter_sse_lines, split_sse_lines


def _splitlines_boundary_chars() -> list[str]:
  """All codepoints below 0x3000 that str.splitlines treats as line boundaries."""
  return [chr(cp) for cp in range(0x3000) if f"a{chr(cp)}b".splitlines() == ["a", "b"]]


def _split_chunked(chunks: list[str]) -> list[str]:
  """Feed str chunks through the pure splitter, threading the buffer like the adapter."""
  lines: list[str] = []
  buffer = ""
  for chunk in chunks:
    emitted, buffer = split_sse_lines(buffer + chunk, final=False)
    lines.extend(emitted)
  emitted, _ = split_sse_lines(buffer, final=True)
  lines.extend(emitted)
  return lines


async def _drain_lines(chunks: list[bytes]) -> list[str]:
  return [line async for line in iter_sse_lines(FakeChunkedResponse(chunks))]


@pytest.mark.parametrize("terminator", ["\n", "\r\n", "\r"])
def test_each_terminator_form_emits_exactly_one_line(terminator: str) -> None:
  lines, remainder = split_sse_lines("data: x" + terminator + "tail", final=False)
  assert lines == ["data: x"]
  assert remainder == "tail"
  assert "\r" not in lines[0]
  assert "\n" not in lines[0]


@pytest.mark.parametrize("terminator", ["\n", "\r\n", "\r"])
def test_terminators_never_remain_inside_lines(terminator: str) -> None:
  lines, remainder = split_sse_lines("a" + terminator + "b" + terminator, final=True)
  assert lines == ["a", "b"]
  assert remainder == ""


def test_derived_boundary_set_matches_the_known_splitline_boundaries() -> None:
  assert set(_splitlines_boundary_chars()) == {
      "\n",
      "\r",
      "\v",
      "\f",
      "\x1c",
      "\x1d",
      "\x1e",
      "\x85",
      "\u2028",
      "\u2029",
  }


_NON_CRLF_BOUNDARIES = [c for c in _splitlines_boundary_chars() if c not in ("\r", "\n")]


@pytest.mark.parametrize(
    "char",
    _NON_CRLF_BOUNDARIES,
    ids=[f"U+{ord(c):04X}" for c in _NON_CRLF_BOUNDARIES],
)
def test_non_crlf_splitline_boundary_is_line_content(char: str) -> None:
  line = "data: a" + char + "b"
  lines, remainder = split_sse_lines(line + "\n", final=False)
  assert lines == [line]
  assert remainder == ""
  lines, remainder = split_sse_lines(line, final=True)
  assert lines == [line]
  assert remainder == ""


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


def test_bare_cr_split_across_chunks_yields_two_lines() -> None:
  assert _split_chunked(["data: x\r", "data: y\n"]) == ["data: x", "data: y"]


def test_empty_lines_are_preserved_as_frame_delimiters() -> None:
  lines, remainder = split_sse_lines("data: a\n\ndata: b\n", final=False)
  assert lines == ["data: a", "", "data: b"]
  assert remainder == ""


def test_empty_buffer_yields_no_lines() -> None:
  assert split_sse_lines("", final=False) == ([], "")
  assert split_sse_lines("", final=True) == ([], "")


def test_final_flush_emits_unterminated_tail_line() -> None:
  lines, remainder = split_sse_lines("data: tail", final=True)
  assert lines == ["data: tail"]
  assert remainder == ""


def test_trailing_cr_at_stream_end_terminates_the_line() -> None:
  lines, remainder = split_sse_lines("data: tail\r", final=True)
  assert lines == ["data: tail"]
  assert remainder == ""


def test_scanned_to_resumes_past_proven_clean_bytes() -> None:
  # First call sees no terminator: the cursor rule over its remainder keeps the
  # held-back trailing CR inside the search range.
  emitted, rem = split_sse_lines("abc\r", final=False)
  assert (emitted, rem) == ([], "abc\r")
  scanned_to = len(rem) - (1 if rem.endswith("\r") else 0)
  # The resumed search must still split the CRLF and emit a line that spans
  # from the buffer start, not from the cursor.
  lines, rem = split_sse_lines(rem + "\ndef", final=False, scanned_to=scanned_to)
  assert (lines, rem) == (["abc"], "def")


def test_scanned_to_default_reproduces_the_full_rescan() -> None:
  # scanned_to=0 is the full-buffer search every caller used before resumption existed.
  buffer = "data: a\r\ndata: b\ntail"
  assert split_sse_lines(buffer, final=False) == split_sse_lines(buffer, final=False, scanned_to=0)
  assert split_sse_lines(buffer, final=True) == split_sse_lines(buffer, final=True, scanned_to=0)


def test_every_two_way_chunk_split_preserves_the_whole_stream_lines() -> None:
  stream = "data: a\r\ndata: b\ndata: c\r\r\n\n"
  expected = ["data: a", "data: b", "data: c", "", ""]
  for cut in range(len(stream) + 1):
    assert _split_chunked([stream[:cut], stream[cut:]]) == expected, cut


def test_one_char_per_chunk_preserves_lines() -> None:
  stream = "data: a\r\ndata: b\ndata: c\r\r\n\n"
  assert _split_chunked(list(stream)) == ["data: a", "data: b", "data: c", "", ""]


def test_production_frame_shape_with_raw_boundary_chars_survives_chunking() -> None:
  nel = "\x85"
  ls = "\u2028"
  part_text = '...identifier\\");else{a:48<' + nel + ">NEL-CHAR" + ls + '>LS-CHAR"},"status":"running",...'
  payload = json.dumps(
      {"type": "message.part.updated", "properties": {"part": {"text": part_text}}},
      ensure_ascii=False,
  )
  assert nel in payload and ls in payload
  bs = chr(92)
  assert (bs * 3 + '"') in payload and (bs + '"status' + bs + '"') in payload
  data_line = "data: " + payload
  stream = data_line + "\n\n"
  cut_nel = stream.index(nel) + 1
  cut_ls = stream.index(ls)
  lines = _split_chunked([stream[:cut_nel], stream[cut_nel:cut_ls], stream[cut_ls:]])
  assert lines == [data_line, ""]
  assert json.loads(lines[0][len("data: "):])["properties"]["part"]["text"] == part_text


@pytest.mark.asyncio
async def test_adapter_yields_lines_across_byte_chunks() -> None:
  chunks = [b": keep\n", b'data: {"a', b'": 1}\n\n', b"data: [done]"]
  assert await _drain_lines(chunks) == [": keep", 'data: {"a": 1}', "", "data: [done]"]


@pytest.mark.asyncio
async def test_adapter_handles_crlf_straddling_chunks() -> None:
  assert await _drain_lines([b"data: x\r", b"\ndata: y\r\n"]) == ["data: x", "data: y"]


@pytest.mark.asyncio
async def test_adapter_decodes_multibyte_character_split_across_chunks() -> None:
  line = "data: caf\u00e9"
  wire = (line + "\n").encode("utf-8")
  cut = wire.index(b"\xc3") + 1
  assert await _drain_lines([wire[:cut], wire[cut:]]) == [line]


@pytest.mark.asyncio
async def test_adapter_flushes_unterminated_final_line() -> None:
  assert await _drain_lines([b"data: tail"]) == ["data: tail"]


@pytest.mark.asyncio
async def test_adapter_replaces_invalid_utf8_bytes() -> None:
  assert await _drain_lines([b"data: \xff\n"]) == ["data: \ufffd"]


@pytest.mark.asyncio
async def test_resumable_adapter_scan_matches_naive_reference_on_every_two_way_split() -> None:
  # The adapter resumes the terminator search instead of re-scanning the whole
  # remainder per chunk; the naive pure-splitter threading is the reference.
  stream = "data: a\r\ndata: b\ndata: c\r\r\n\n"
  wire = stream.encode()
  for cut in range(len(wire) + 1):
    assert await _drain_lines([wire[:cut], wire[cut:]]) == _split_chunked([stream[:cut], stream[cut:]]), cut


@pytest.mark.asyncio
async def test_adapter_frames_random_byte_chunkings_like_one_buffer() -> None:
  # Random multi-way byte cuts exercise the piecewise accumulator paths the
  # fixed-cut cases miss: held-back CRs followed by a third chunk, terminators
  # late in a chunk with a pending partial line, and mid-multibyte cuts.
  rng = random.Random(20260902)
  streams = [
      "data: a\r\ndata: b\ndata: c\r\r\n\ndata: caf\u00e9\r\ntail",
      "".join(f"data: token-{i}\n" for i in range(200)) + "data: [done]",
  ]
  for stream in streams:
    expected = _split_chunked([stream])
    wire = stream.encode()
    for _ in range(50):
      cuts = sorted(rng.sample(range(1, len(wire)), 8))
      bounds = [0, *cuts, len(wire)]
      chunks = [wire[a:b] for a, b in zip(bounds, bounds[1:])]
      assert await _drain_lines(chunks) == expected


@pytest.mark.asyncio
async def test_large_frame_spanning_many_chunks_frames_like_one_buffer() -> None:
  # The resumable-scan worst case in production: multi-chunk frames with the
  # terminator only at the frame end.
  frame = "data: " + "x" * 100_000
  raw = (frame + "\r\n\r\n" + frame + "\n\n").encode()
  chunks = [raw[i:i + 4096] for i in range(0, len(raw), 4096)]
  assert await _drain_lines(chunks) == [frame, "", frame, ""]
