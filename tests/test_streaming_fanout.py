"""Focused tests for StreamingManager fan-out: stream coalescing and serialize-once."""

import asyncio
import json

import pytest

from src.core import streaming
from src.core.streaming import StreamingManager

WINDOW = 0.05


class _Socket:
  """WebSocket double recording every send_text payload; sends stay ordered."""

  def __init__(self) -> None:
    self.texts: list[str] = []
    self.closed = False

  async def send_text(self, text: str) -> None:
    self.texts.append(text)

  async def close(self) -> None:
    self.closed = True

  def payloads(self) -> list[dict]:
    return [json.loads(t) for t in self.texts]


def _stream(content: str) -> dict:
  return {"type": "stream", "message": {"role": "assistant", "content": content}}


@pytest.fixture
def manager(monkeypatch: pytest.MonkeyPatch) -> StreamingManager:
  monkeypatch.setattr(streaming, "_STREAM_COALESCE_INTERVAL", WINDOW)
  return StreamingManager()


async def _settle() -> None:
  await asyncio.sleep(WINDOW + 0.06)


@pytest.mark.asyncio
async def test_stream_frames_lead_then_coalesce_to_latest(manager: StreamingManager) -> None:
  ws = _Socket()
  await manager.subscribe("s", ws)
  await manager.broadcast("s", _stream("one"))
  assert ws.payloads() == [_stream("one")]
  await manager.broadcast("s", _stream("two"))
  await manager.broadcast("s", _stream("three"))
  assert ws.payloads() == [_stream("one")]
  await _settle()
  await manager.broadcast("s", _stream("four"))
  assert ws.payloads() == [_stream("one"), _stream("three"), _stream("four")]


@pytest.mark.asyncio
async def test_preview_hiding_frame_drops_pending(manager: StreamingManager) -> None:
  ws = _Socket()
  await manager.subscribe("s", ws)
  await manager.broadcast("s", _stream("one"))
  await manager.broadcast("s", _stream("two"))
  await manager.broadcast("s", {"type": "error", "message": "boom"})
  await _settle()
  assert ws.payloads() == [_stream("one"), {"type": "error", "message": "boom"}]


@pytest.mark.asyncio
async def test_preview_neutral_frame_keeps_pending(manager: StreamingManager) -> None:
  ws = _Socket()
  await manager.subscribe("s", ws)
  await manager.broadcast("s", _stream("one"))
  await manager.broadcast("s", {"type": "thinking", "content": "..."})
  await manager.broadcast("s", _stream("two"))
  await _settle()
  assert ws.payloads() == [_stream("one"), {"type": "thinking", "content": "..."}, _stream("two")]


@pytest.mark.asyncio
async def test_serialize_once_per_fan_out_over_subscribers(
    manager: StreamingManager, monkeypatch: pytest.MonkeyPatch) -> None:
  render_calls = 0
  real_render = streaming.fast_json_bytes

  def counting_render(content: object) -> bytes:
    nonlocal render_calls
    render_calls += 1
    return real_render(content)

  monkeypatch.setattr(streaming, "fast_json_bytes", counting_render)
  ws1, ws2 = _Socket(), _Socket()
  await manager.subscribe("s", ws1)
  await manager.subscribe("s", ws2)
  await manager.broadcast("s", {"type": "error", "message": "boom"})
  assert render_calls == 1 and ws1.texts == ws2.texts


@pytest.mark.asyncio
async def test_wire_render_parsed_equal_to_stdlib_send_json_form(manager: StreamingManager) -> None:
  # The fan-out replaced the stdlib send_json render; the frames' parsed
  # content must equal that form, with raw UTF-8 where both renders agree.
  ws = _Socket()
  await manager.subscribe("s", ws)
  frame = {"type": "stream", "message": {"role": "assistant", "content": "中文 \U0001F600 tail"}}
  await manager.broadcast("s", frame)
  assert ws.payloads() == [frame]
  assert ws.texts[0] == json.dumps(frame, separators=(",", ":"), ensure_ascii=False)


@pytest.mark.asyncio
async def test_wire_render_nan_boundary(manager: StreamingManager) -> None:
  # A NaN float rides the wire as null — valid JSON the client parses — where
  # the replaced stdlib form shipped the invalid literal NaN.
  ws = _Socket()
  await manager.subscribe("s", ws)
  await manager.broadcast("s", {"type": "diag", "ratio": float("nan")})
  assert ws.payloads() == [{"type": "diag", "ratio": None}]
  json.loads(ws.texts[0])  # the wire text must stay parseable


@pytest.mark.asyncio
async def test_wire_render_non_str_key_raises(manager: StreamingManager) -> None:
  # A non-str dict key raises at the fan-out instead of the stdlib's silent
  # str coercion, so a malformed frame surfaces at its producer.
  ws = _Socket()
  await manager.subscribe("s", ws)
  with pytest.raises(TypeError):
    await manager.broadcast("s", {"type": "diag", 7: "x"})
  assert ws.texts == []


@pytest.mark.asyncio
async def test_unsubscribe_close_all_and_subscriberless_drop_pending(manager: StreamingManager) -> None:
  ws = _Socket()
  await manager.subscribe("s", ws)
  await manager.broadcast("s", _stream("one"))
  await manager.broadcast("s", _stream("two"))
  await manager.unsubscribe("s", ws)
  other = _Socket()
  await manager.subscribe("t", other)
  await manager.broadcast("t", _stream("a"))
  await manager.broadcast("t", _stream("b"))
  await manager.broadcast("u", _stream("never scheduled without subscribers"))
  await manager.close_all()
  await _settle()
  assert ws.payloads() == [_stream("one")]
  assert other.payloads() == [_stream("a")]
  assert other.closed
  assert manager._stream_timers == {}
  assert manager._pending_stream == {}
