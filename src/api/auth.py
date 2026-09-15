"""Bearer-token authentication middleware for CharlieBot."""

import hmac
import json
from http.cookies import CookieError, SimpleCookie

from starlette.requests import Request
from starlette.types import ASGIApp, Receive, Scope, Send

from src.core.config import configured_access_key
from src.core.constants import FILE_SERVER_MOUNTS
from src.core.run_token import RunTokenError, bearer_from_authorization, verify_run_token


def _credential_matches(candidate: str, key: str) -> bool:
  """Constant-time comparison — the one place that owns the credential comparison."""
  return hmac.compare_digest(candidate, key)


# The cookie the login page sets (see _LOGIN_PAGE); its JS strings below must
# keep the literal name because they are served HTML.
_ACCESS_KEY_COOKIE = "charliebot_access_key"


def _credential_accepted(bearer: str, cookie: str, key: str) -> bool:
  """One home of the acceptance decision: bearer or cookie must match *key* in constant time."""
  return (bool(bearer) and _credential_matches(bearer, key)) or (bool(cookie) and _credential_matches(cookie, key))


def request_has_access_key(request: Request, key: str) -> bool:
  """True when *request* carries a valid access key, or when *key* is empty.

  An empty configured key means the middleware passes every request through,
  so every reader counts as authenticated. Otherwise the key is accepted from
  either an ``Authorization: Bearer`` header or the access-key cookie,
  compared with ``hmac.compare_digest``.
  """
  if not key:
    return True
  return _credential_accepted(_bearer_from_scope(request.scope), request.cookies.get(_ACCESS_KEY_COOKIE, ""), key)


# Paths that are always public (no auth required). The viewer routes only render
# or re-serve data already public via the file server, so exposing them leaks nothing
# new and makes trace/report links shareable.
_PUBLIC_PATHS = frozenset({"/", "/perfetto", "/perfetto/merged", "/ncu", "/api/auth/status"})
_PUBLIC_PREFIXES = ("/static/", *(mount + "/" for mount in FILE_SERVER_MOUNTS))

# Self-contained HTML login page served to unauthenticated browser navigations.
# On submit it stores the key in localStorage (the source of truth for the SPA
# fetch wrapper and the terminal WS ?token=) AND sets the charliebot_access_key
# cookie, which is the only credential a browser auto-sends on a top-level
# navigation, then reloads. SameSite=Strict closes the CSRF surface cookie auth
# would otherwise open; Secure is set on https (Tailscale) and omitted on plain
# http, where the browser refuses Secure cookies — the loopback session-tree
# preview serves plain http, and its login must survive the reload.
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
      // The cookie carries the credential on top-level navigations. SameSite=Strict; Secure
      // only on https: — a loopback-HTTP deployment (the session-tree preview) cannot set
      // Secure cookies, and without the cookie every top-level navigation would loop back
      // to this login page.
      var cookieAttrs = 'path=/; SameSite=Strict' + (location.protocol === 'https:' ? '; Secure' : '');
      document.cookie = 'charliebot_access_key=' + k + '; ' + cookieAttrs;
      location.reload();
      return false;
    }
  </script>
</body>
</html>"""


def _header_value(scope: Scope, name: bytes) -> str:
  """First value of header *name* from the raw ASGI scope, or "" (starlette Headers.get parity)."""
  for header_name, value in scope["headers"]:
    if header_name == name:
      return value.decode("latin-1")
  return ""


def _bearer_from_scope(scope: Scope) -> str:
  return bearer_from_authorization(_header_value(scope, b"authorization"))


def _cookie_key_from_scope(scope: Scope) -> str:
  raw = b";".join(value for name, value in scope["headers"] if name == b"cookie")
  if not raw:
    return ""
  jar = SimpleCookie()
  try:
    jar.load(raw.decode("latin-1"))
  except CookieError:
    return ""
  morsel = jar.get(_ACCESS_KEY_COOKIE)
  return morsel.value if morsel else ""


def _scope_bearer_is_run_token(bearer: str, key: str) -> bool:
  """Signature-only run-token check (the active-Run binding is enforced per route).

  A bearer that is not the access key is treated as run-token use: a valid
  signature passes the middleware and the caller-identity dependency
  (``src.api.deps.require_caller``) binds it to an active Run. An invalid one
  fails closed here — it never falls back to the operator cookie.
  """
  if not bearer or not key:
    return False
  try:
    verify_run_token(bearer, key)
  except RunTokenError:
    return False
  return True


def _scope_has_access_key(scope: Scope, key: str) -> bool:
  bearer = _bearer_from_scope(scope)
  if bearer:
    if key and _credential_matches(bearer, key):
      return True
    # A presented non-access-key bearer is run-token use: verified or rejected,
    # the operator cookie is never consulted for it.
    return _scope_bearer_is_run_token(bearer, key)
  return _credential_accepted("", _cookie_key_from_scope(scope), key)


async def _send_unauthorized(send: Send, html: bool) -> None:
  if html:
    body = _LOGIN_PAGE.encode("utf-8")
    content_type = b"text/html; charset=utf-8"
  else:
    body = json.dumps({"detail": "Unauthorized"}).encode("utf-8")
    content_type = b"application/json"
  headers = [(b"content-type", content_type), (b"content-length", str(len(body)).encode("latin-1"))]
  await send({"type": "http.response.start", "status": 401, "headers": headers})
  await send({"type": "http.response.body", "body": body})


class AuthMiddleware:
  """Reject HTTP requests that lack a valid access key.

  The key is accepted from either an ``Authorization: Bearer`` header or a
  ``charliebot_access_key`` cookie. Unauthenticated browser navigations get an
  HTML login page; other unauthenticated requests get a JSON 401. When
  ``charliebot_access_key`` is empty the middleware is a no-op (all requests
  pass through).

  Pure ASGI, not BaseHTTPMiddleware: the middleware rides every request, and
  the BaseHTTPMiddleware wrapper's per-request task plus anyio memory streams
  are the M3 middleware-floor overhead this form removes.
  """

  def __init__(self, app: ASGIApp) -> None:
    self.app = app

  async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
    if scope["type"] != "http":
      await self.app(scope, receive, send)
      return
    key = configured_access_key()
    path = scope["path"]

    # Let public paths through without auth.
    if not key or path in _PUBLIC_PATHS or path.startswith(_PUBLIC_PREFIXES):
      await self.app(scope, receive, send)
      return

    # Accept the access key from either the Authorization: Bearer header (used by
    # the SPA fetch wrapper) or the charliebot_access_key cookie (the only
    # credential a browser auto-sends on a top-level navigation).
    if _scope_has_access_key(scope, key):
      await self.app(scope, receive, send)
      return

    # Unauthenticated. Serve the HTML login page to browser navigations so the
    # user can authenticate; keep the bare JSON 401 for API/fetch calls.
    if scope["method"] == "GET" and "text/html" in _header_value(scope, b"accept"):
      await _send_unauthorized(send, html=True)
      return
    await _send_unauthorized(send, html=False)
