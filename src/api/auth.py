"""Bearer-token authentication middleware for CharlieBot."""

import hmac
import json

from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request
from starlette.responses import Response

from src.core.config import get_config

# Paths that are always public (no auth required).
_PUBLIC_PATHS = frozenset({"/", "/api/auth/status"})
_PUBLIC_PREFIXES = ("/static/",)


class AuthMiddleware(BaseHTTPMiddleware):
  """Reject HTTP requests that lack a valid Bearer token.

  When ``charliebot_access_key`` is empty the middleware is a no-op
  (all requests pass through).
  """

  async def dispatch(self, request: Request, call_next) -> Response:
    cfg = get_config()
    key = cfg.charliebot_access_key
    if not key:
      return await call_next(request)

    path = request.url.path

    # Let public paths through without auth.
    if path in _PUBLIC_PATHS or any(path.startswith(p) for p in _PUBLIC_PREFIXES):
      return await call_next(request)

    # Check Authorization header.
    auth_header = request.headers.get("authorization", "")
    if auth_header.startswith("Bearer "):
      token = auth_header[7:]
    else:
      token = ""

    if not token or not hmac.compare_digest(token, key):
      return Response(
          content=json.dumps({"detail": "Unauthorized"}),
          status_code=401,
          media_type="application/json",
      )

    return await call_next(request)
