"""Backend stream funnels parse stream lines through orjson.

The stream-side parse funnels — the spawned-stdout NDJSON reader and the
raw-log tail-follow loop — ride orjson under the ndjson reader skip contract's
boundary: a line that strips to empty is invisible, a line the parser rejects
yields nothing, and the stdlib json NaN/Infinity extensions plus double-
overflow floats skip as malformed. Machine-written stream lines carry none of
those literals; the opencode SSE and anthropic-proxy readers keep their
existing raise-on-malformed contract, only the parser moves.
"""

from __future__ import annotations

import tempfile
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
async def test_tail_follow_events_carries_partial_line_across_chunks() -> None:
  """A line straddling the 64 KB read boundary yields exactly once: the
  trailing partial is carried into the next chunk, never processed half."""
  payload = b'{"type": "assistant", "seq": 9, "pad": "' + b"x" * 2000 + b'"}\n'
  events = await _collect_tail_events(b"\n" * 65530 + payload, post_result_timeout=60.0)
  assert [event["seq"] for event in events] == [9]


@pytest.mark.asyncio
async def test_tail_follow_events_drops_torn_final_line() -> None:
  """A final line the producer never finished stays unprocessed (the torn
  final write replays as at most a duplicate — never a loss)."""
  torn = b'{"type": "assistant", "seq": 1}\n{"type": "assistant", "seq": 2'
  events = await _collect_tail_events(torn, post_result_timeout=60.0)
  assert [event["seq"] for event in events] == [1]


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
