"""Backend stream funnels parse stream lines through orjson.

The stream-side parse funnels — the spawned-stdout NDJSON reader and the
raw-log tail-follow loop — ride orjson under the ndjson reader skip contract's
boundary: an empty or whitespace-only line is invisible, a line the parser
rejects yields nothing, and the stdlib json NaN/Infinity extensions plus
double-overflow floats skip as malformed. Machine-written stream lines carry
none of those literals; the opencode SSE and anthropic-proxy readers keep
their existing raise-on-malformed contract, only the parser moves.
"""

from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path
from typing import Any

import pytest
from conftest import _async_wait_for, cancel_and_drain

from src.agents.backends.base import iter_ndjson_events, tail_follow_events

_LINES = [
    b'{"type": "assistant", "seq": 1}\n',
    b"\n",
    b"not json at all\n",
    b'{"type": "user", "seq": 2, "note": NaN}\n',
    b'{"type": "result", "seq": 3}\n',
]

_ASSISTANT_LINE_BYTES = len(_LINES[0]) + len(_LINES[1]) + len(_LINES[2])


class _ChunkReader:

  """A read()-shaped stand-in for the piped stdout StreamReader.

  The feed cuts the stream at *chunk_size* bytes mid-line on purpose: the
  funnel's line split must reassemble lines across read boundaries.
  """

  def __init__(self, stream: bytes, chunk_size: int) -> None:
    self._stream = stream
    self._chunk_size = chunk_size
    self._at = 0

  async def read(self, size: int) -> bytes:
    del size  # the transport's own pace decides; the funnel's ask never shrinks a feed
    chunk = self._stream[self._at:self._at + self._chunk_size]
    self._at += len(chunk)
    return chunk


@pytest.mark.asyncio
async def test_iter_ndjson_events_parses_and_skips() -> None:
  stream = b"".join(_LINES)
  reader = _ChunkReader(stream, chunk_size=7)
  events = [event async for event in iter_ndjson_events(reader)]
  assert [event["seq"] for event in events] == [1, 3]


@pytest.mark.asyncio
async def test_iter_ndjson_events_raises_past_the_stream_buffer_limit(monkeypatch: pytest.MonkeyPatch) -> None:
  """One line past the stream buffer limit raises the readline shape's
  ValueError instead of buffering without bound (the runaway-line bound)."""
  from src.agents.backends import base

  monkeypatch.setattr(base, "DEFAULT_BUFFER_LIMIT", 64)
  reader = _ChunkReader(b'{"pad": "' + b"x" * 100, chunk_size=1 << 20)
  with pytest.raises(ValueError, match="buffer limit"):
    [event async for event in iter_ndjson_events(reader)]


async def _collect_tail_events(raw_bytes: bytes, **kwargs: Any) -> list[dict]:
  """Write *raw_bytes* as the raw log and return every event tail_follow_events yields."""
  with tempfile.TemporaryDirectory() as work:
    raw = Path(work) / "agent.raw.ndjson"
    raw.write_bytes(raw_bytes)
    return [
        event async for event in tail_follow_events(
            raw,
            translate=lambda event: [event],
            is_alive=lambda: False,
            **kwargs,
        )
    ]


async def _collect_staged_tail(partial: bytes, completion: bytes) -> list[dict]:
  """Write *partial*, let the follow consume it, then append *completion.

  The follow runs until the completed line lands, so the test drives the
  real multi-round read shape: the first round carries the trailing partial,
  the second round's read completes it. A completed line in *partial* would
  defeat the staging, so the caller passes a byte string with no newline.
  """
  with tempfile.TemporaryDirectory() as work:
    raw = Path(work) / "agent.raw.ndjson"
    assert b"\n" not in partial
    raw.write_bytes(partial)
    events: list[dict] = []

    async def consume() -> None:
      # Per-item append is load-bearing: the mid-flight `assert events == []` below
      # probes incremental delivery, which a collect-then-extend defers to the end.
      async for event in tail_follow_events(
          raw,
          translate=lambda event: [event],
          is_alive=lambda: True,
          post_result_timeout=9999.0,
      ):
        events.append(event)  # noqa: PERF401  (see comment above)

    task = asyncio.create_task(consume())
    try:
      await asyncio.sleep(0.1)  # the follow's first read rounds over the partial
      assert events == []
      with raw.open("ab") as f:
        f.write(completion)
      await _async_wait_for(lambda: bool(events), 2.0, "the follow never consumed the appended line")
    finally:
      await cancel_and_drain(task)
    return events


@pytest.mark.asyncio
async def test_tail_follow_events_replays_from_offset() -> None:
  """The re-attach shape: a restart resumes at the recorded byte offset."""
  events = await _collect_tail_events(b"".join(_LINES), start_offset=_ASSISTANT_LINE_BYTES, post_result_timeout=60.0)

  # The NaN-bearing line lands in this range and skips as malformed (the
  # parser boundary the funnels adopt), so only the result line survives.
  assert [event["seq"] for event in events] == [3]


@pytest.mark.asyncio
async def test_tail_follow_events_checkpoints_cursor_at_consumed_offset() -> None:
  """A mount with a cursor file leaves it at the consumed byte offset —
  including the skipped lines' bytes (blank and malformed consume index), so
  a re-attach at the recorded offset replays nothing already delivered."""
  from src.core import runs

  raw_bytes = b"".join(_LINES)
  with tempfile.TemporaryDirectory() as work:
    raw = Path(work) / "agent.raw.ndjson"
    raw.write_bytes(raw_bytes)
    cursor = Path(work) / runs.CURSOR_NAME
    events = [
        event async for event in tail_follow_events(
            raw,
            translate=lambda event: [event],
            is_alive=lambda: False,
            cursor=cursor,
            post_result_timeout=60.0,
        )
    ]
    assert [event["seq"] for event in events] == [1, 3]
    assert runs.read_raw_cursor(cursor) == len(raw_bytes)
    # A re-attach at the recorded offset replays nothing.
    events = [
        event async for event in tail_follow_events(
            raw,
            translate=lambda event: [event],
            is_alive=lambda: False,
            cursor=cursor,
            start_offset=runs.read_raw_cursor(cursor),
            post_result_timeout=60.0,
        )
    ]
    assert events == []


@pytest.mark.asyncio
async def test_tail_follow_events_carries_partial_line_across_read_rounds() -> None:
  """A line written in two appends yields exactly once: the first round's
  trailing partial rides the carry into the next round's read, never
  processed half."""
  events = await _collect_staged_tail(b'{"type": "assistant", "seq": 9, "pad": "', b'xx"}\n')
  assert [event["seq"] for event in events] == [9]


@pytest.mark.asyncio
async def test_tail_follow_events_drops_torn_final_line() -> None:
  """A final line the producer never finished stays unprocessed (the torn
  final write replays as at most a duplicate — never a loss)."""
  torn = b'{"type": "assistant", "seq": 1}\n{"type": "assistant", "seq": 2'
  events = await _collect_tail_events(torn, post_result_timeout=60.0)
  assert [event["seq"] for event in events] == [1]
