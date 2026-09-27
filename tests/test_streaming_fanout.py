"""Focused tests for StreamingManager fan-out: stream coalescing and serialize-once."""

import json

import pytest

from src.api import responses
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


@pytest.fixture
def manager(monkeypatch: pytest.MonkeyPatch) -> StreamingManager:
  monkeypatch.setattr(streaming, "_STREAM_COALESCE_INTERVAL", WINDOW)
  return StreamingManager()


@pytest.mark.asyncio
async def test_serialize_once_per_fan_out_over_subscribers(
    manager: StreamingManager, monkeypatch: pytest.MonkeyPatch) -> None:
  render_calls = 0
  real_render = responses.fast_json_bytes

  def counting_render(content: object) -> bytes:
    nonlocal render_calls
    render_calls += 1
    return real_render(content)

  monkeypatch.setattr(responses, "fast_json_bytes", counting_render)
  ws1, ws2 = _Socket(), _Socket()
  await manager.subscribe("s", ws1)
  await manager.subscribe("s", ws2)
  await manager.broadcast("s", {"type": "error", "message": "boom"})
  assert render_calls == 1 and ws1.texts == ws2.texts


@pytest.mark.asyncio
async def test_wire_render_non_str_key_raises(manager: StreamingManager) -> None:
  # A non-str dict key raises at the fan-out instead of the stdlib's silent
  # str coercion, so a malformed frame surfaces at its producer.
  ws = _Socket()
  await manager.subscribe("s", ws)
  with pytest.raises(TypeError):
    await manager.broadcast("s", {"type": "diag", 7: "x"})
  assert ws.texts == []
