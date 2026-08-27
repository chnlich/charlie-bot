"""Bearer-token authentication middleware for CharlieBot."""

import hmac
import json

from starlette.middleware.base import BaseHTTPMiddleware, RequestResponseEndpoint
from starlette.requests import Request
from starlette.responses import HTMLResponse, Response

from src.core.config import get_config


def request_has_access_key(request: Request, key: str) -> bool:
  """True when *request* carries a valid access key, or when *key* is empty.

  An empty configured key means the middleware passes every request through,
  so every reader counts as authenticated. Otherwise the key is accepted from
  either an ``Authorization: Bearer`` header or a ``charliebot_access_key``
  cookie, compared with ``hmac.compare_digest`` — the one place that owns the
  credential comparison.
  """
  if not key:
    return True
  auth_header = request.headers.get("authorization", "")
  bearer = auth_header[7:] if auth_header.startswith("Bearer ") else ""
  cookie = request.cookies.get("charliebot_access_key", "")
  if (bearer and hmac.compare_digest(bearer, key)) or (cookie and hmac.compare_digest(cookie, key)):
    return True
  return False


# Paths that are always public (no auth required). The viewer routes only render
# or re-serve data already public via "/files/", so exposing them leaks nothing
# new and makes trace/report links shareable.
_PUBLIC_PATHS = frozenset({"/", "/perfetto", "/perfetto/merged", "/ncu", "/api/auth/status"})
_PUBLIC_PREFIXES = ("/static/", "/files/", "/absolute_filepath/")

# Self-contained HTML login page served to unauthenticated browser navigations.
# On submit it stores the key in localStorage (the source of truth for the SPA
# fetch wrapper and the terminal WS ?token=) AND sets the charliebot_access_key
# cookie, which is the only credential a browser auto-sends on a top-level
# navigation, then reloads. SameSite=Strict closes the CSRF surface cookie auth
# would otherwise open; Secure is appropriate since the server is reached only
# over HTTPS (Tailscale).
_LOGIN_PAGE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CharlieBot</title>
<style>
  body { margin:0; height:100vh; display:flex; align-items:center; justify-content:center;
         background:#0f172a; color:#e2e8f0; font-family:system-ui,sans-serif; }
  .box { width:100%; max-width:24rem; padding:0 1.5rem; text-align:center; }
  h1 { color:#60a5fa; font-size:1.25rem; margin:0 0 1.5rem; }
  p { color:#94a3b8; font-size:.875rem; margin:0 0 1rem; }
  input, button { width:100%; box-sizing:border-box; border-radius:.5rem;
                  padding:.625rem 1rem; font-size:.875rem; }
  input { background:#1e293b; border:1px solid #475569; color:#e2e8f0; margin-bottom:.75rem; }
  input:focus { outline:none; border-color:#3b82f6; }
  button { background:#2563eb; color:#fff; border:none; font-weight:500; cursor:pointer; }
  button:hover { background:#3b82f6; }
</style>
</head>
<body>
  <div class="box">
    <h1>CharlieBot</h1>
    <p>Enter access key to continue</p>
    <form onsubmit="return unlock(event)">
      <input id="k" type="password" placeholder="Access key" autofocus>
      <button type="submit">Unlock</button>
    </form>
  </div>
  <script>
    function unlock(e) {
      e.preventDefault();
      var k = document.getElementById('k').value.trim();
      if (!k) return false;
      // localStorage is the source of truth for the SPA fetch wrapper and the terminal WS ?token=.
      localStorage.setItem('charliebot_access_key', k);
      // The cookie carries the credential on top-level navigations. SameSite=Strict + Secure;
      // see _LOGIN_PAGE comment above for why.
      document.cookie = 'charliebot_access_key=' + k + '; path=/; SameSite=Strict; Secure';
      location.reload();
      return false;
    }
  </script>
</body>
</html>"""


class AuthMiddleware(BaseHTTPMiddleware):
  """Reject HTTP requests that lack a valid access key.

  The key is accepted from either an ``Authorization: Bearer`` header or a
  ``charliebot_access_key`` cookie. Unauthenticated browser navigations get an
  HTML login page; other unauthenticated requests get a JSON 401. When
  ``charliebot_access_key`` is empty the middleware is a no-op (all requests
  pass through).
  """

  async def dispatch(self, request: Request, call_next: RequestResponseEndpoint) -> Response:
    cfg = get_config()
    key = cfg.charliebot_access_key
    if not key:
      return await call_next(request)

    path = request.url.path

    # Let public paths through without auth.
    if path in _PUBLIC_PATHS or any(path.startswith(p) for p in _PUBLIC_PREFIXES):
      return await call_next(request)

    # Accept the access key from either the Authorization: Bearer header (used by
    # the SPA fetch wrapper) or the charliebot_access_key cookie (the only
    # credential a browser auto-sends on a top-level navigation).
    if request_has_access_key(request, key):
      return await call_next(request)

    # Unauthenticated. Serve the HTML login page to browser navigations so the
    # user can authenticate; keep the bare JSON 401 for API/fetch calls.
    accept = request.headers.get("accept", "")
    if request.method == "GET" and "text/html" in accept:
      return HTMLResponse(content=_LOGIN_PAGE, status_code=401)
    return Response(
        content=json.dumps({"detail": "Unauthorized"}),
        status_code=401,
        media_type="application/json",
    )
