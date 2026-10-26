"""Scheduled-handler registry: packages register scheduled-task handlers by name.

Each registration holds a module path and an attribute name. The scheduler imports the
module when a task fires, so a handler's stack loads only when it runs.
"""

from __future__ import annotations

import importlib

from src.infra import registration

_HANDLERS: dict[str, tuple[str, str]] = {}  # name -> (module, attr)


def register_handler(name: str, module: str, *, attr: str) -> None:
  """A registered task with `handler: <name>` awaits getattr(import_module(module), attr)().

  The coroutine returns the fire's summary string. A second registration of one name raises ValueError.
  """
  if name in _HANDLERS:
    raise registration.already_registered("handler", name, _HANDLERS[name][0])
  _HANDLERS[name] = (module, attr)


def handler(name: str) -> object | None:
  """The registered handler coroutine function for name, imported; None when no package registered name."""
  if name not in _HANDLERS:
    return None
  module, attr = _HANDLERS[name]
  return getattr(importlib.import_module(module), attr)
