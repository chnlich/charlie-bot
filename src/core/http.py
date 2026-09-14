"""Shared httpx.AsyncClient singleton for outbound HTTP requests.

httpx imports lazily on first use: the server import floor (docs/perf_baseline.md
M99) must not pay httpx's import chain (~60 ms with rich) for a client that only
outbound requests touch.
"""

from typing import TYPE_CHECKING

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
