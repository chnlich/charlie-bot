"""Every route resolves config through the on-loop dependency, never a sync one.

``Depends(get_config)`` hands FastAPI a sync callable, which it resolves through
a threadpool round-trip per request; ``get_config_on_loop`` awaits the same
memoized instance on the event loop. The polled routes were migrated first; this
test pins the rest so no route reintroduces the hop.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable, Iterator
from typing import Any

from fastapi.dependencies.models import Dependant
from fastapi.routing import APIRoute

import server
from src.api.deps import get_config_on_loop
from src.core.config import get_config


def _dependency_calls(dependant: Dependant) -> Iterator[Callable[..., Any]]:
  yield dependant.call
  for sub in dependant.dependencies:
    yield from _dependency_calls(sub)


def test_no_route_depends_on_sync_get_config() -> None:
  offenders = [
      f"{sorted(route.methods)} {route.path}" for route in server.app.routes if isinstance(route, APIRoute)
      for call in _dependency_calls(route.dependant) if call is get_config
  ]
  assert offenders == [], (
      "routes resolving config through the sync dependency (threadpool hop per "
      f"request); switch them to Depends(get_config_on_loop): {offenders}")


def test_on_loop_dependency_is_the_async_form() -> None:
  # The guard above is only as good as the wrapper it points at: the on-loop
  # dependency must stay a coroutine, or FastAPI is back on the threadpool.
  assert asyncio.iscoroutinefunction(get_config_on_loop)
