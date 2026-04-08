"""Shared httpx.AsyncClient singleton for outbound HTTP requests."""

import httpx

_client: httpx.AsyncClient | None = None


def get_http_client() -> httpx.AsyncClient:
  """Return the shared AsyncClient, creating it lazily on first call."""
  global _client
  if _client is None:
    _client = httpx.AsyncClient()
  return _client


async def close_http_client() -> None:
  """Close the shared AsyncClient. Call from app shutdown."""
  global _client
  if _client is not None:
    await _client.aclose()
    _client = None
