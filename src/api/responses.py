"""JSON response rendering for the request-path endpoints.

The hot JSON endpoints return pre-built plain payloads, so the remaining
per-request cost is the render itself. Starlette's JSONResponse renders with
``ensure_ascii=False``, and CPython's C JSON encoder is several times slower
on non-ASCII-bearing payloads in that mode (~3x measured on a 559 KB CJK-heavy
chat page; the sessions corpus on this deployment is Chinese-heavy). Rendering
ASCII-escaped keeps the parsed content identical and is never slower, also on
ASCII-only payloads.
"""

import json
from typing import Any

from fastapi.responses import JSONResponse


class FastJsonResponse(JSONResponse):
  """JSONResponse whose render escapes non-ASCII as \\uXXXX escapes.

  Callers rely on: the parsed content equals the ensure_ascii=False render
  (\\uXXXX decodes to the same string; only the raw bytes differ, and the
  escaped body is ASCII), and a NaN/Infinity payload raises ValueError at
  render time instead of emitting invalid JSON (Starlette's allow_nan=False
  contract).
  """

  def render(self, content: Any) -> bytes:
    return json.dumps(
        content,
        ensure_ascii=True,
        allow_nan=False,
        indent=None,
        separators=(",", ":"),
    ).encode("utf-8")
