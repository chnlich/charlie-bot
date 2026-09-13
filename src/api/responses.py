"""JSON response rendering for the request-path endpoints.

The hot JSON endpoints return pre-built plain payloads, so the remaining
per-request cost is the render itself. orjson renders the same payload
parsed-identically several times faster than CPython's C JSON encoder and
emits raw UTF-8 instead of ``\\uXXXX`` escapes, shrinking non-ASCII-bearing
bodies on the wire. Callers rely on: the parsed content equals the stdlib
render, splices built from ``fast_json_bytes`` segments stay byte-identical
to a fresh render of the merged payload (every side renders through this one
function), and unsupported payload types raise loudly at render time.

Two deliberate serializer boundaries ride orjson, both test-pinned: a
NaN/Infinity float renders as null — valid JSON, so the wire never breaks,
the same boundary the stream funnels accepted at their orjson swap — and a
non-str dict key raises TypeError instead of the stdlib's silent str
coercion.
"""

from typing import Any

import orjson
from fastapi.responses import JSONResponse
from starlette.responses import Response


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
  its output), so served bodies stay byte-identical to the FastJsonResponse
  render of the same payload.
  """

  media_type = "application/json"

  def __init__(self, body: bytes) -> None:
    super().__init__(content=body)
