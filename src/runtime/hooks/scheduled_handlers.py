"""Scheduled-handler registry: packages register cron handlers by name and the loop action.

Each registration holds a module path and an attribute name. The scheduler imports the
module when a task fires, so a handler's stack loads only when it runs.
"""

from __future__ import annotations

import importlib

_HANDLERS: dict[str, tuple[str, str]] = {}  # name -> (module, attr)
_loop_action: tuple[str, str] | None = None


def register_handler(name: str, module: str, *, attr: str) -> None:
  """A cron.d task with `handler: <name>` awaits getattr(import_module(module), attr)().

  The coroutine returns the fire's summary string. A second registration of one name raises ValueError.
  """
  if name in _HANDLERS:
    raise ValueError(f"handler {name!r} is already registered by {_HANDLERS[name][0]}")
  _HANDLERS[name] = (module, attr)


def register_loop_action(module: str, *, attr: str) -> None:
  """A cron.d task with a `loop:` section awaits getattr(import_module(module), attr)(task_cfg) on each fire.

  The coroutine returns (action, prompt). action is a short label for the log; prompt None means
  this fire has nothing to do. A second registration raises ValueError.
  """
  global _loop_action
  if _loop_action is not None:
    raise ValueError(f"the loop action is already registered by {_loop_action[0]}")
  _loop_action = (module, attr)


def handler(name: str) -> object | None:
  """The registered handler coroutine function for name, imported; None when no package registered name."""
  if name not in _HANDLERS:
    return None
  module, attr = _HANDLERS[name]
  return getattr(importlib.import_module(module), attr)


def loop_action() -> object:
  """The registered loop action, imported. Raises LookupError when no package registered one."""
  if _loop_action is None:
    raise LookupError("no package registered a loop action")
  module, attr = _loop_action
  return getattr(importlib.import_module(module), attr)
