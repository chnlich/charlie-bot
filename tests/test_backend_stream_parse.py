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
import contextlib
import tempfile
import time
from pathlib import Path
from typing import Any

import pytest
from test_opencode_backend import _build_backend, _FakeSseResponse

from src.agents.backends.base import iter_ndjson_events, tail_follow_events

_LINES = [
    b'{"type": "assistant", "seq": 1}\n',
    b"\n",
    b"not json at all\n",
    b'{"type": "user", "seq": 2, "note": NaN}\n',
    b'{"type": "result", "seq": 3}\n',
]

_ASSISTANT_LINE_BYTES = len(_LINES[0]) + len(_LINES[1]) + len(_LINES[2])


class _LineReader:

  def __init__(self, lines: list[bytes]) -> None:
    self._lines = lines
    self._i = 0

  def __aiter__(self) -> "_LineReader":
    return self

  async def __anext__(self) -> bytes:
    if self._i >= len(self._lines):
      raise StopAsyncIteration
    line = self._lines[self._i]
    self._i += 1
    return line


@pytest.mark.asyncio
async def test_iter_ndjson_events_parses_and_skips() -> None:
  events = [event async for event in iter_ndjson_events(_LineReader(_LINES))]
  assert [event["seq"] for event in events] == [1, 3]


@pytest.mark.asyncio
async def test_iter_ndjson_events_torn_bytes_parse_as_replacement_char() -> None:
  """A complete line carrying a torn multibyte char parses as U+FFFD through
  the skip contract's replace fallback; the funnel never decodes valid lines."""
  lines = [b'{"type": "assistant", "seq": 1, "note": "ok\xff"}\n', b'{"type": "result", "seq": 2}\n']
  events = [event async for event in iter_ndjson_events(_LineReader(lines))]
  assert events[0]["note"] == "ok\ufffd"
  assert [event["seq"] for event in events] == [1, 2]


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
      async for event in tail_follow_events(
          raw,
          translate=lambda event: [event],
          is_alive=lambda: True,
          post_result_timeout=9999.0,
      ):
        events.append(event)

    task = asyncio.create_task(consume())
    try:
      await asyncio.sleep(0.1)  # the follow's first read rounds over the partial
      assert events == []
      with raw.open("ab") as f:
        f.write(completion)
      deadline = time.monotonic() + 2.0
      while not events and time.monotonic() < deadline:
        await asyncio.sleep(0.01)
    finally:
      task.cancel()
      with contextlib.suppress(asyncio.CancelledError):
        await task
    return events


@pytest.mark.asyncio
async def test_tail_follow_events_parses_and_skips() -> None:
  events = await _collect_tail_events(b"".join(_LINES), post_result_timeout=60.0)
  assert [event["seq"] for event in events] == [1, 3]


@pytest.mark.asyncio
async def test_tail_follow_events_replays_from_offset() -> None:
  """The re-attach shape: a restart resumes at the recorded byte offset."""
  events = await _collect_tail_events(b"".join(_LINES), start_offset=_ASSISTANT_LINE_BYTES, post_result_timeout=60.0)

  # The NaN-bearing line lands in this range and skips as malformed (the
  # parser boundary the funnels adopt), so only the result line survives.
  assert [event["seq"] for event in events] == [3]


@pytest.mark.asyncio
async def test_tail_follow_events_carries_partial_line_across_read_rounds() -> None:
  """A line written in two appends yields exactly once: the first round's
  trailing partial rides the carry into the next round's read, never
  processed half."""
  events = await _collect_staged_tail(b'{"type": "assistant", "seq": 9, "pad": "', b'xx"}\n')
  assert [event["seq"] for event in events] == [9]


@pytest.mark.asyncio
async def test_tail_follow_events_carries_multimegabyte_line_across_read_rounds() -> None:
  """A multi-MB line written in two appends yields exactly once: the carry
  joins the next round's read in one pass, so the cost stays linear in the
  line's bytes (the live raw log carries multi-MB events)."""
  partial = b'{"type": "assistant", "seq": 9, "pad": "' + b"x" * (1024 * 1024)
  events = await _collect_staged_tail(partial, b'"}\n')
  assert [event["seq"] for event in events] == [9]


@pytest.mark.asyncio
async def test_tail_follow_events_drops_torn_final_line() -> None:
  """A final line the producer never finished stays unprocessed (the torn
  final write replays as at most a duplicate — never a loss)."""
  torn = b'{"type": "assistant", "seq": 1}\n{"type": "assistant", "seq": 2'
  events = await _collect_tail_events(torn, post_result_timeout=60.0)
  assert [event["seq"] for event in events] == [1]


@pytest.mark.asyncio
async def test_tail_follow_events_warns_torn_bytes_only_for_the_partial() -> None:
  """The torn-tail warning names the unprocessed partial's bytes, and a log
  ending on a completed line warns nothing."""
  from structlog.testing import capture_logs

  complete = b'{"type": "assistant", "seq": 1}\n'
  torn_tail = b'{"type": "assistant", "seq": 2'
  with capture_logs() as logs:
    events = await _collect_tail_events(complete, post_result_timeout=60.0)
  assert [event["seq"] for event in events] == [1]
  assert not [entry for entry in logs if entry.get("event") == "raw_trailing_torn_line_dropped"]

  with capture_logs() as logs:
    events = await _collect_tail_events(complete + torn_tail, post_result_timeout=60.0)
  assert [event["seq"] for event in events] == [1]
  warnings = [entry for entry in logs if entry.get("event") == "raw_trailing_torn_line_dropped"]
  assert len(warnings) == 1
  assert warnings[0]["bytes"] == len(torn_tail)


@pytest.mark.asyncio
async def test_opencode_sse_events_rejects_nan_boundary(monkeypatch) -> None:
  """The stdlib parser accepted NaN literals; orjson fails the frame loudly
  (the boundary the stream funnels deliberately adopt, as the file readers
  did — machine-written upstream events carry none)."""
  backend = _build_backend(monkeypatch)
  response = _FakeSseResponse(["data: " + '{"type": "session.idle", "note": NaN}', ""])

  with pytest.raises(ValueError):
    async for _ in backend._iter_sse_events(response):
      pass
