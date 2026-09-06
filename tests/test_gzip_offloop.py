import asyncio
import gzip

from fastapi import FastAPI
from fastapi.responses import StreamingResponse
from starlette.middleware.gzip import GZipMiddleware
from starlette.responses import Response

import server

BODY = ("{\"text\": \"" + "内容" * 60_000 + "\"}").encode("utf-8")
SCOPE = {
    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
    "http_version": "1.1", "method": "GET", "scheme": "http",
    "path": "/big", "raw_path": b"/big", "query_string": b"",
    "root_path": "", "headers": [(b"host", b"t"), (b"accept-encoding", b"gzip")],
    "client": ("t", 1), "server": ("t", 80),
}


def _build(handler, middleware) -> FastAPI:
  app = FastAPI()
  app.get("/big")(handler)
  app.add_middleware(middleware, minimum_size=1000, compresslevel=6)
  return app


def _drive(app: FastAPI) -> tuple[dict, bytes]:
  headers: dict = {}
  body = b""
  done = asyncio.Event()

  async def send(message):
    nonlocal body
    if message["type"] == "http.response.start":
      headers.update({k.decode(): v.decode() for k, v in message["headers"]})
    else:
      body += message.get("body", b"")
      if not message.get("more_body"):
        done.set()

  sent = False

  async def receive():
    # StreamingResponse parks a listener on receive() until http.disconnect; a
    # real server blocks there until the client goes away, so the stream wins
    # the race. Unblock only once the final body chunk has passed the send side.
    nonlocal sent
    if not sent:
      sent = True
      return {"type": "http.request", "body": b"", "more_body": False}
    await done.wait()
    return {"type": "http.disconnect"}

  asyncio.run(app(SCOPE, receive, send))
  return headers, body


def test_whole_body_gzip_bytes_match_starlette_inline():
  def handler():
    return Response(content=BODY, media_type="application/json")

  headers, body = _drive(_build(handler, server._CharlieBotGZipMiddleware))
  baseline_headers, baseline_body = _drive(_build(handler, GZipMiddleware))

  assert headers["content-encoding"] == "gzip"
  assert body == baseline_body
  assert gzip.decompress(body) == BODY


def test_small_body_and_preset_encoding_stay_identity():
  def small():
    return Response(content=b"{}", media_type="application/json")

  headers, body = _drive(_build(small, server._CharlieBotGZipMiddleware))
  assert "content-encoding" not in headers
  assert body == b"{}"

  def preset():
    return Response(content=BODY, media_type="application/gzip", headers={"Content-Encoding": "gzip"})

  headers, body = _drive(_build(preset, server._CharlieBotGZipMiddleware))
  assert headers["content-encoding"] == "gzip"
  assert body == BODY


def test_streaming_body_compresses_per_chunk():
  def stream():
    return StreamingResponse(
        (BODY[i:i + 100_000] for i in range(0, len(BODY), 100_000)),
        media_type="application/json")

  headers, body = _drive(_build(stream, server._CharlieBotGZipMiddleware))
  baseline_headers, baseline_body = _drive(_build(stream, GZipMiddleware))

  assert headers["content-encoding"] == "gzip"
  assert body == baseline_body
  assert gzip.decompress(body) == BODY
