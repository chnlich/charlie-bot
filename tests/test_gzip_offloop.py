import asyncio
import gzip
from collections.abc import AsyncIterator, Callable
from typing import Any

from conftest import make_http_scope
from fastapi import FastAPI
from fastapi.responses import StreamingResponse
from starlette.middleware.gzip import GZipMiddleware
from starlette.responses import Response
from starlette.types import Message

import server

BODY = ("{\"text\": \"" + "内容" * 60_000 + "\"}").encode("utf-8")


def _strip_mtime(wire: bytes) -> bytes:
  # gzip.GzipFile embeds the construction wall time in header bytes 4..8, so
  # two wires generated seconds apart differ there while every compressed byte
  # matches; compare the payloads with that field zeroed out.
  if wire[:3] != b"\x1f\x8b\x08":
    return wire
  return wire[:4] + b"\x00\x00\x00\x00" + wire[8:]


def _build(handler: Any, middleware: Any) -> FastAPI:
  app = FastAPI()
  app.get("/big")(handler)
  # Level 3 is the responder's non-default ceiling: the mounted compresslevel
  # must be an ISA-L level (0-3) since the responder's file is an IGzipFile,
  # where unlike zlib a level of 0 is not "store".
  app.add_middleware(middleware, minimum_size=1000, compresslevel=3)
  return app


def _sliced_stream(media_type: str) -> Callable[[], StreamingResponse]:
  """A handler streaming BODY in 100 KB slices. Each call builds a fresh
  response and generator: the same handler is driven through two middlewares,
  and a second drive over an exhausted generator would read an empty body."""

  def stream() -> StreamingResponse:

    async def chunks() -> AsyncIterator[bytes]:
      for i in range(0, len(BODY), 100_000):
        yield BODY[i:i + 100_000]

    return StreamingResponse(chunks(), media_type=media_type)

  return stream


def _drive(app: FastAPI) -> tuple[dict[str, str], bytes]:
  headers: dict[str, str] = {}
  body = b""
  done = asyncio.Event()

  async def send(message: Message) -> None:
    nonlocal body
    if message["type"] == "http.response.start":
      headers.update({k.decode(): v.decode() for k, v in message["headers"]})
    else:
      body += message.get("body", b"")
      if not message.get("more_body"):
        done.set()

  sent = False

  async def receive() -> Message:
    # StreamingResponse parks a listener on receive() until http.disconnect; a
    # real server blocks there until the client goes away, so the stream wins
    # the race. Unblock only once the final body chunk has passed the send side.
    nonlocal sent
    if not sent:
      sent = True
      return {"type": "http.request", "body": b"", "more_body": False}
    await done.wait()
    return {"type": "http.disconnect"}

  scope = make_http_scope("/big", headers=[(b"host", b"t"), (b"accept-encoding", b"gzip")])
  asyncio.run(app(scope, receive, send))
  return headers, body


def test_whole_body_gzip_bytes_match_starlette_inline() -> None:

  def handler() -> Response:
    return Response(content=BODY, media_type="application/json")

  headers, body = _drive(_build(handler, server._CharlieBotGZipMiddleware))
  _, baseline_body = _drive(_build(handler, GZipMiddleware))

  assert headers["content-encoding"] == "gzip"
  # The responder deflates with ISA-L and the stock middleware with zlib, so
  # the wires differ as byte streams; the pinned contracts are the container's
  # validity, the parsed-content parity, and the off-loop hop changing no
  # bytes of the responder's own wire.
  assert gzip.decompress(body) == BODY
  assert gzip.decompress(baseline_body) == BODY
  _, body_again = _drive(_build(handler, server._CharlieBotGZipMiddleware))
  assert _strip_mtime(body) == _strip_mtime(body_again)


def test_small_body_and_preset_encoding_stay_identity() -> None:

  def small() -> Response:
    return Response(content=b"{}", media_type="application/json")

  headers, body = _drive(_build(small, server._CharlieBotGZipMiddleware))
  assert "content-encoding" not in headers
  assert body == b"{}"

  def preset() -> Response:
    return Response(content=BODY, media_type="application/gzip", headers={"Content-Encoding": "gzip"})

  headers, body = _drive(_build(preset, server._CharlieBotGZipMiddleware))
  assert headers["content-encoding"] == "gzip"
  assert body == BODY


def test_streaming_body_compresses_per_chunk() -> None:
  stream = _sliced_stream("application/json")

  headers, body = _drive(_build(stream, server._CharlieBotGZipMiddleware))
  _, baseline_body = _drive(_build(stream, GZipMiddleware))

  assert headers["content-encoding"] == "gzip"
  # Same deflator split as the whole-body test above: the streamed chunks'
  # wire is ISA-L's, the stock middleware's is zlib's, so parity is parsed.
  assert gzip.decompress(body) == BODY
  assert gzip.decompress(baseline_body) == BODY


def test_already_compressed_media_types_ride_identity() -> None:

  def page() -> Response:
    return Response(content=BODY, media_type="image/png")

  headers, body = _drive(_build(page, server._CharlieBotGZipMiddleware))
  assert "content-encoding" not in headers
  assert body == BODY

  deck = _sliced_stream("application/vnd.openxmlformats-officedocument.presentationml.presentation")

  headers, body = _drive(_build(deck, server._CharlieBotGZipMiddleware))
  assert "content-encoding" not in headers
  assert body == BODY


def test_text_media_types_keep_compressing() -> None:

  def svg() -> Response:
    return Response(content=BODY, media_type="image/svg+xml")

  headers, body = _drive(_build(svg, server._CharlieBotGZipMiddleware))
  assert headers["content-encoding"] == "gzip"
  assert gzip.decompress(body) == BODY

  def events() -> StreamingResponse:

    async def chunks() -> AsyncIterator[bytes]:
      yield BODY
      yield b""

    return StreamingResponse(chunks(), media_type="text/event-stream")

  headers, body = _drive(_build(events, server._CharlieBotGZipMiddleware))
  assert "content-encoding" not in headers
  assert body == BODY + b""
