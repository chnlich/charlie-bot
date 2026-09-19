"""JSON response rendering for the request-path endpoints.

The hot JSON endpoints return these Response subclasses directly, not plain
dicts, so FastAPI skips response_model validation and the jsonable_encoder
pass it runs on mapped returns. Their payloads are pre-built plain values, so
the remaining per-request cost is the render itself. orjson renders the same
payload parsed-identically several times faster than CPython's C JSON encoder
and emits raw UTF-8 instead of ``\\uXXXX`` escapes, shrinking non-ASCII-bearing
bodies on the wire. Callers rely on: the parsed content equals the stdlib
render, splices built from ``fast_json_bytes`` segments stay byte-identical
to a fresh render of the merged payload, and unsupported payload types raise
loudly at render time.

Two deliberate serializer boundaries ride orjson, both test-pinned: a
NaN/Infinity float renders as null — valid JSON, so the wire never breaks,
the same boundary the stream funnels accepted at their orjson swap — and a
non-str dict key raises TypeError instead of the stdlib's silent str
coercion.

The splice byte-identity holds because orjson renders a dict context-free in
the dict's own key order, and the splice sites' hand-rolled scalar pieces
(src/api/sessions.py) render byte-identically to orjson's for their
code-fixed types (None, bool, int, ASCII strings).
"""

import asyncio
from typing import Any

import orjson
from fastapi.responses import JSONResponse
from starlette.requests import Request
from starlette.responses import Response

from src.core.compression import gzip_level1
from src.core.memo import BoundedMemo


def fast_json_bytes(content: Any) -> bytes:
  """Render *content* to the FastJsonResponse body bytes.

  Callers rely on: the parsed content equals the stdlib render (only the raw
  bytes differ — raw UTF-8 where the stdlib emitted ``\\uXXXX`` escapes), the
  compact no-space separators, and a NaN/Infinity float rendering as null
  rather than invalid JSON.
  """
  return orjson.dumps(content)


class FastJsonResponse(JSONResponse):
  """JSONResponse whose render goes through :func:`fast_json_bytes`."""

  def render(self, content: Any) -> bytes:
    return fast_json_bytes(content)


class PreencodedJSONResponse(Response):
  """JSON response serving body bytes a caller already rendered.

  The bytes must come from :func:`fast_json_bytes` (directly or via a memo of
  its output), or be a reversible transform of such bytes — the events page's
  gzip form — so the served parsed content stays identical to the
  FastJsonResponse render of the same payload.
  """

  media_type = "application/json"

  def __init__(self, body: bytes, headers: dict[str, str] | None = None) -> None:
    super().__init__(content=body, headers=headers)


GZIP_RESPONSE_HEADERS: dict[str, str] = {"Content-Encoding": "gzip", "Vary": "Accept-Encoding"}


def request_wants_gzip(request: Request) -> bool:
  """Whether the client's Accept-Encoding admits gzip.

  The same check the gzip middleware makes on the way in; answering with the
  pre-compressed body and :data:`GZIP_RESPONSE_HEADERS` set is what makes that
  middleware skip its per-request deflate.
  """
  return "gzip" in request.headers.get("accept-encoding", "")


async def gzip_body_response(
    request: Request, body: bytes, headers: dict[str, str], memo: BoundedMemo[bytes, bytes]) -> Response:
  """Serve *body* plain or from *memo*'s gzip form (keyed on the bytes themselves).

  One off-loop level-1 deflate per distinct body, stored so a repeat serves
  the stored bytes; the gzip headers make the middleware skip (see
  :func:`request_wants_gzip`).
  """
  if not request_wants_gzip(request):
    return PreencodedJSONResponse(body, headers=headers)
  gz = memo.get(body)
  if gz is None:
    gz = await asyncio.to_thread(gzip_level1, body)
    memo.store(body, gz)
  return PreencodedJSONResponse(gz, headers={**headers, **GZIP_RESPONSE_HEADERS})
