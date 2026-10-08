"""Wiring registry: packages register their routers, CLI commands, background services, file views, startup checks
and diff roots.

Each registration holds module path strings. The server and the CLI import a registered
module when they use it, so this module imports nothing heavy and a CLI command loads
only its own module.
"""

from __future__ import annotations

import importlib
from collections.abc import Callable
from typing import TYPE_CHECKING

if TYPE_CHECKING:
  from pathlib import Path

  from src.infra.config import CharlieBotConfig

PHASES = ("early", "ready")

# Registration order is the order of the lists: routers include in it, services start in it.
_ROUTERS: list[tuple[str, str, tuple[str, ...], str, bool]] = []  # (module, prefix, tags, attr, before_runtime)
_COMMANDS: dict[str, str] = {}
_SERVICES: dict[str, tuple[str, str]] = {}  # name -> (module, phase)
_FILE_VIEWS: list[tuple[str, str]] = []  # (module, attr)
_STARTUP_CHECKS: list[tuple[str, str]] = []  # (module, attr)
_DIFF_ROOTS: list[tuple[str, str]] = []  # (module, attr)


class ServiceContext:
  """What the server hands each service's start_service."""
  __slots__ = ("app", "cfg", "recovery_task", "session_mgr")

  def __init__(self, app, cfg, session_mgr, recovery_task) -> None:
    # app: the FastAPI app; cfg: CharlieBotConfig; session_mgr: SessionManager;
    # recovery_task: the lifespan's crash-recovery asyncio.Task.
    self.app = app
    self.cfg = cfg
    self.session_mgr = session_mgr
    self.recovery_task = recovery_task


def register_router(
    module: str,
    *,
    prefix: str = "",
    tags: tuple[str, ...] = (),
    attr: str = "router",
    before_runtime: bool = False) -> None:
  """The server includes getattr(import_module(module), attr) under prefix, in registration order.

  A router registered with before_runtime includes ahead of the runtime's own routers, so a fixed
  path under a runtime prefix answers before the runtime's parameterised route that would match it
  (/api/sessions/{session_id}). The others include after them.
  """
  _ROUTERS.append((module, prefix, tags, attr, before_runtime))


def register_command(name: str, module: str) -> None:
  """`charliebot <name>` imports module and calls its main(). A second registration of one name raises ValueError."""
  if name in _COMMANDS:
    raise ValueError(f"command {name!r} is already registered by {_COMMANDS[name]}")
  _COMMANDS[name] = module


def register_service(name: str, module: str, *, phase: str = "ready") -> None:
  """module defines `async def start_service(ctx: ServiceContext) -> None` and `async def stop_service() -> None`.

  phase "early" starts right after the crash-recovery task is created; phase "ready" starts
  after trigger recovery. A second registration of one name, or a phase outside PHASES,
  raises ValueError.
  """
  if phase not in PHASES:
    raise ValueError(f"service {name!r} has phase {phase!r}; the phases are {PHASES}")
  if name in _SERVICES:
    raise ValueError(f"service {name!r} is already registered by {_SERVICES[name][0]}")
  _SERVICES[name] = (module, phase)


def register_file_view(module: str, *, attr: str) -> None:
  """The file server awaits getattr(import_module(module), attr)(request, path) before it serves a path.

  path is the route's path parameter: the absolute filesystem path without its leading "/".
  The view returns a Response to answer the request, or None to pass. It may raise
  fastapi.HTTPException. Views run in registration order; the first Response answers.
  A second registration of one (module, attr) raises ValueError.
  """
  if (module, attr) in _FILE_VIEWS:
    raise ValueError(f"file view {module}:{attr} is already registered")
  _FILE_VIEWS.append((module, attr))


def register_startup_check(module: str, *, attr: str) -> None:
  """The server calls getattr(import_module(module), attr)(cfg) once at start, before it serves.

  cfg is the CharlieBotConfig. The function raises ValueError to stop the start. Checks run
  in registration order. A second registration of one (module, attr) raises ValueError.
  """
  if (module, attr) in _STARTUP_CHECKS:
    raise ValueError(f"startup check {module}:{attr} is already registered")
  _STARTUP_CHECKS.append((module, attr))


def register_diff_root(module: str, *, attr: str) -> None:
  """The diff view also accepts repositories under getattr(import_module(module), attr)(cfg), a Path.

  cfg is the CharlieBotConfig. The roots join the paths.workspace_dirs roots in registration order.
  A second registration of one (module, attr) raises ValueError.
  """
  if (module, attr) in _DIFF_ROOTS:
    raise ValueError(f"diff root {module}:{attr} is already registered")
  _DIFF_ROOTS.append((module, attr))


def routers(*, before_runtime: bool = False) -> tuple[tuple[str, str, tuple[str, ...], str], ...]:
  """(module, prefix, tags, attr) in registration order, for the routers registered with this before_runtime."""
  return tuple(router[:4] for router in _ROUTERS if router[4] == before_runtime)


def commands() -> dict[str, str]:
  """name -> module."""
  return dict(_COMMANDS)


def service_starts(phase: str) -> list[tuple[str, object]]:
  """(name, start_service) for one phase, in registration order; imports each service module.

  Each start_service is an async function that takes a ServiceContext.
  """
  return [
      (name, importlib.import_module(module).start_service)
      for name, (module, service_phase) in _SERVICES.items()
      if service_phase == phase
  ]


def service_stops() -> list[tuple[str, object]]:
  """(name, stop_service) for every registered service: "ready" ones in reverse registration order, then "early" ones in reverse.

  Each stop_service is an async function that takes no argument.
  """
  stops = []
  for phase in reversed(PHASES):
    stops.extend(
        (name, importlib.import_module(module).stop_service)
        for name, (module, service_phase) in reversed(_SERVICES.items())
        if service_phase == phase)
  return stops


def file_views() -> list[object]:
  """The registered view coroutine functions, imported, in registration order."""
  return [getattr(importlib.import_module(module), attr) for module, attr in _FILE_VIEWS]


def startup_checks() -> list[Callable[[CharlieBotConfig], None]]:
  """The registered startup check functions, imported, in registration order."""
  return [getattr(importlib.import_module(module), attr) for module, attr in _STARTUP_CHECKS]


def diff_roots() -> list[Callable[[CharlieBotConfig], Path]]:
  """The registered diff root functions, imported, in registration order."""
  return [getattr(importlib.import_module(module), attr) for module, attr in _DIFF_ROOTS]
