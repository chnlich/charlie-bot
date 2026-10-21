"""The registries that backend packages fill from their ``register()`` for the backend lifecycle hook.

There are four: child-process environment edits, shared context limits by reading kind, context limits by backend
type and reading kind, and usage resolvers by backend type. ``src/runtime/hooks/backend_lifecycle.py`` holds the
lifecycle types that the launch loop runs. Every registered target is a "module:attr" string imported on first use,
so registering costs no backend import. This module imports no backend module, no ``dataclasses`` and no heavy module.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from src.infra import deferred

if TYPE_CHECKING:
  from src.infra import models
  from src.runtime.hooks import backend_lifecycle

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
    deferred.import_attr(fn)(env)


# ---------------------------------------------------------------------------
# Context limits
# ---------------------------------------------------------------------------

_reading_limits_fns: dict[str, str] = {}
_backend_reading_limits_fns: dict[tuple[str, str], str] = {}


def register_reading_limits(reading_kind: str, fn: str) -> None:
  """Register ``fn``, a "module:function" string: ``function(run_kind) -> ContextLimits`` for ``reading_kind``."""
  if reading_kind in _reading_limits_fns:
    raise ValueError(f"reading limits for {reading_kind!r} are already registered")
  _reading_limits_fns[reading_kind] = fn


def register_backend_reading_limits(backend_type: str, reading_kind: str, fn: str) -> None:
  """Register ``fn(option, run_kind) -> ContextLimits`` for one backend type and reading kind.

  This provider takes precedence over ``register_reading_limits`` when a caller supplies an option of the type.
  """
  key = (backend_type, reading_kind)
  if key in _backend_reading_limits_fns:
    raise ValueError(f"backend reading limits for {key!r} are already registered")
  _backend_reading_limits_fns[key] = fn


def reading_limits(
    reading_kind: str,
    run_kind: models.RunKind,
    option: models.BackendOption | None = None,
) -> backend_lifecycle.ContextLimits | None:
  """The limits of ``reading_kind`` for a run of *run_kind*, computed per call.

  A type-specific provider takes precedence when *option* is supplied. None
  when no package registered the reading kind.
  """
  if option is not None:
    backend_fn = _backend_reading_limits_fns.get((option.type, reading_kind))
    if backend_fn is not None:
      return deferred.import_attr(backend_fn)(option, run_kind)
  fn = _reading_limits_fns.get(reading_kind)
  if fn is None:
    return None
  return deferred.import_attr(fn)(run_kind)


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
  return deferred.import_attr(cls)
