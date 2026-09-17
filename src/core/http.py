"""Shared outbound-HTTP helpers: the httpx.AsyncClient singleton and the deferred
requests loader.

Both third-party imports defer to first use: no import floor (server M99, CLI
M92 in docs/perf_baseline.md) pays an HTTP library's chain for paths that never
send a request (~60 ms httpx with rich, ~100 ms requests via urllib3 +
charset_normalizer).
"""

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
  import httpx

_client: "httpx.AsyncClient | None" = None


def get_http_client() -> "httpx.AsyncClient":
  """Return the shared AsyncClient, creating it lazily on first call."""
  global _client
  if _client is None:
    import httpx

    _client = httpx.AsyncClient()
  return _client


async def close_http_client() -> None:
  """Close the shared AsyncClient. Call from app shutdown."""
  global _client
  if _client is not None:
    await _client.aclose()
    _client = None


def load_requests(namespace: dict[str, Any]) -> Any:
  """Bind requests into *namespace* on first use and return it.

  Each consumer passes its own ``globals()``: the per-module binding keeps that
  module's bare-name reads working and its module-attribute route (e.g.
  ``src.core.artifact_wrap.requests``) the tests' monkeypatch target, exactly as
  ``load_croniter`` does for croniter.
  """
  import requests

  namespace["requests"] = requests
  return requests

