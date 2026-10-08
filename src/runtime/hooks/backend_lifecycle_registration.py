"""The registries that backend packages fill from their ``register()`` for the backend lifecycle hook.

There are three: the child-process environment edits, the context limits of a reading kind, and the usage
resolver of a backend type. ``src/runtime/hooks/backend_lifecycle.py`` holds the lifecycle types that the launch
loop runs. Every registered target is a "module:attr" string imported on first use, so registering costs no
backend import. This module imports no backend module, no ``dataclasses`` and no heavy module.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from src.infra.deferred import import_attr

if TYPE_CHECKING:
  from src.runtime.hooks.backend_lifecycle import ContextLimits

# ---------------------------------------------------------------------------
# Child-process environment
# ---------------------------------------------------------------------------

_child_env_fns: list[str] = []


def register_child_env(fn: str) -> None:
  """Register ``fn``, a "module:function" string: ``function(env: dict[str, str]) -> None`` edits the env in place.

  The runtime applies every registered function, in registration order, to the environment of
  every agent child process.
  """
  if fn in _child_env_fns:
    raise ValueError(f"child env function {fn!r} is already registered")
  _child_env_fns.append(fn)


def apply_child_env(env: dict[str, str]) -> None:
  """Apply every registered child-env function to ``env``, in registration order."""
  for fn in _child_env_fns:
    import_attr(fn)(env)


# ---------------------------------------------------------------------------
# Context limits
# ---------------------------------------------------------------------------

_reading_limits_fns: dict[str, str] = {}


def register_reading_limits(reading_kind: str, fn: str) -> None:
  """Register ``fn``, a "module:function" string: ``function() -> ContextLimits`` for ``reading_kind``."""
  if reading_kind in _reading_limits_fns:
    raise ValueError(f"reading limits for {reading_kind!r} are already registered")
  _reading_limits_fns[reading_kind] = fn


def reading_limits(reading_kind: str) -> ContextLimits | None:
  """The limits of ``reading_kind``, computed per call; None when no package registered the kind."""
  fn = _reading_limits_fns.get(reading_kind)
  if fn is None:
    return None
  return import_attr(fn)()


# ---------------------------------------------------------------------------
# Usage resolvers
# ---------------------------------------------------------------------------

_usage_resolver_classes: dict[str, str] = {}


def register_usage_resolver(backend_type: str, cls: str) -> None:
  """Register ``cls``, a "module:Class" string, as the usage resolver of ``backend_type``.

  ``Class(cfg, events_cache, chat_events_path_fn)`` has ``resolve(session_id, session_meta, events)``,
  which returns a usage dict or None.
  """
  if backend_type in _usage_resolver_classes:
    raise ValueError(f"usage resolver for {backend_type!r} is already registered")
  _usage_resolver_classes[backend_type] = cls


def usage_resolver_for(backend_type: str) -> type | None:
  """The usage resolver class of ``backend_type``; None when the type has none."""
  cls = _usage_resolver_classes.get(backend_type)
  if cls is None:
    return None
  return import_attr(cls)
