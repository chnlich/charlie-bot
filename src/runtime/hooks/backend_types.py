"""The backend type table: how the runtime builds and judges each backend type.

A backend package registers its type here from its ``register()`` function (the packages listed
in ``src/app/registrations.py``). The runtime builds backends, reads resume and relay behavior,
and reads context limits through this table and imports no backend module.

Vocabulary:

- A *type* is a ``BackendOption.type`` string such as "codex".
- The *option model* of a type is the pydantic model of its ``backends.options[]`` entry
  (``src/infra/config_registry.py`` holds it).
- The *factory* of a type builds its ``AgentBackend`` from one option.
- The *traits* of a type are the behaviors that runtime code reads by type, in ``BackendTraits``.
- The *lifecycle* of a type is its ``BackendLifecycle`` (``backend_lifecycle.py``).

The option model, the factory and the lifecycle are "module:attr" strings. They import on first
use, so a registration costs no backend import.
"""

from __future__ import annotations

import dataclasses
from typing import TYPE_CHECKING, Any

from src.infra import config_registry
from src.infra.deferred import import_attr

if TYPE_CHECKING:
  from src.infra.config import CharlieBotConfig
  from src.infra.models import BackendOption, SessionMetadata
  from src.runtime.agent_process.base import AgentBackend
  from src.runtime.hooks import backend_lifecycle

RESUME_CLI_FLAG = "cli_flag"
RESUME_NATIVE_ID = "native_id"
_RESUME_STYLES = (RESUME_CLI_FLAG, RESUME_NATIVE_ID)


@dataclasses.dataclass(frozen=True)
class BackendTraits:
  """Immutable per-type record of the behaviors that runtime code reads by backend type.

  ``resume`` is the resume style: "cli_flag" passes ``--resume <id>`` on the command line or
  "native_id" passes the backend's own session id to its constructor.
  ``restart_reattach`` is True when a restarted server can follow the process of an interrupted
  run. ``preassigned_session_id`` is True when the runtime chooses the session id of a task run
  before the process starts. ``reads_context_window`` is True when the backend takes a context
  window from its option. ``family_prefix`` is the id prefix that names the type's family of
  backend ids; None when the type has no family.
  """
  resume: str
  restart_reattach: bool
  preassigned_session_id: bool
  reads_context_window: bool
  family_prefix: str | None

  def __post_init__(self) -> None:
    if self.resume not in _RESUME_STYLES:
      raise ValueError(f"resume style {self.resume!r} is not one of {_RESUME_STYLES}")


@dataclasses.dataclass(frozen=True)
class _Registration:
  factory: str
  traits: BackendTraits
  lifecycle: str | None
  translate_fallback: bool


_registry: dict[str, _Registration] = {}
_lifecycles: dict[str, backend_lifecycle.BackendLifecycle] = {}
_DEFAULT_LIFECYCLE: backend_lifecycle.BackendLifecycle | None = None


def register_backend_type(
    backend_type: str,
    *,
    options: str,
    factory: str,
    traits: BackendTraits,
    lifecycle: str | None = None,
    translate_fallback: bool = False,
) -> None:
  """Register one backend type.

  ``options``, ``factory`` and ``lifecycle`` are "module:attr" strings, imported on first use.
  The options attr is the pydantic option model; this function forwards it to
  ``config_registry.register_option_model``. The factory attr is ``(option, cfg, **launch_kwargs) -> AgentBackend``. The lifecycle attr is
  a ``BackendLifecycle`` subclass, instantiated once with no arguments on first use.
  ``translate_fallback`` marks the one type that serves the worker's binary-free translate
  fallback; its factory then also accepts ``option=None``. A second registration of one type
  raises ValueError.
  """
  if backend_type in _registry:
    raise ValueError(f"backend type {backend_type!r} is already registered")
  if translate_fallback and any(registration.translate_fallback for registration in _registry.values()):
    raise ValueError(f"backend type {backend_type!r} cannot serve the translate fallback: another type does")
  config_registry.register_option_model(backend_type, options)
  _registry[backend_type] = _Registration(
      factory=factory, traits=traits, lifecycle=lifecycle, translate_fallback=translate_fallback)


def _registration(backend_type: str) -> _Registration:
  registration = _registry.get(backend_type)
  if registration is None:
    raise ValueError(f"Unknown backend type: {backend_type}")
  return registration


def build_backend(option: BackendOption, cfg: CharlieBotConfig, **launch_kwargs: Any) -> AgentBackend:
  """Instantiate the AgentBackend of ``option.type``.

  ``launch_kwargs`` go to the factory, which forwards them to the backend constructor (for example
  extra_flags, buffer_limit, on_spawn). Raises ValueError when the type is unregistered or
  required config is missing.
  """
  return import_attr(_registration(option.type).factory)(option, cfg, **launch_kwargs)


def build_translate_fallback(cfg: CharlieBotConfig, **launch_kwargs: Any) -> AgentBackend:
  """Instantiate the backend of the type that serves the binary-free translate fallback."""
  for registration in _registry.values():
    if registration.translate_fallback:
      return import_attr(registration.factory)(None, cfg, **launch_kwargs)
  raise ValueError("no backend type serves the translate fallback")


def traits_for(backend_type: str) -> BackendTraits:
  """The traits of ``backend_type``; ValueError when the type is unregistered."""
  return _registration(backend_type).traits


def _lifecycle_at(path: str | None) -> backend_lifecycle.BackendLifecycle:
  """The lifecycle instance a registered path names; the default lifecycle for None."""
  global _DEFAULT_LIFECYCLE
  from src.runtime.hooks import backend_lifecycle

  if path is None:
    if _DEFAULT_LIFECYCLE is None:
      _DEFAULT_LIFECYCLE = backend_lifecycle.BackendLifecycle()
    return _DEFAULT_LIFECYCLE
  if path not in _lifecycles:
    _lifecycles[path] = import_attr(path)()
  return _lifecycles[path]


def lifecycle_for(option: BackendOption) -> backend_lifecycle.BackendLifecycle:
  """The lifecycle of ``option.type``; the default lifecycle when the type registered none."""
  return _lifecycle_at(_registration(option.type).lifecycle)


def lifecycle_for_type(backend_type: str) -> backend_lifecycle.BackendLifecycle:
  """The lifecycle of ``backend_type``; ValueError when the type is unregistered."""
  return _lifecycle_at(_registration(backend_type).lifecycle)


def lifecycles() -> tuple[backend_lifecycle.BackendLifecycle, ...]:
  """Every registered lifecycle, each once, in registration order; types without one are left out."""
  paths = dict.fromkeys(r.lifecycle for r in _registry.values() if r.lifecycle is not None)
  return tuple(_lifecycle_at(path) for path in paths)


# The account label is a fact of the session, not of one turn: a turn on any backend carries the
# label of the pool login that holds the session's transcript. These functions read and write it
# through every lifecycle that keeps one.


def account_keeper(meta: SessionMetadata) -> backend_lifecycle.BackendLifecycle | None:
  """The lifecycle whose account label ``meta`` carries; None when ``meta`` carries none."""
  return next((lifecycle for lifecycle in lifecycles() if lifecycle.account_label(meta) is not None), None)


def record_account_label(meta: SessionMetadata, label: str | None) -> bool:
  """Record ``label`` on ``meta`` through every lifecycle that keeps account labels; True when any label changed."""
  return any([lifecycle.record_account_label(meta, label) for lifecycle in lifecycles()])


def type_for_family_prefix(backend_id: str) -> str | None:
  """The type whose ``family_prefix`` starts *backend_id*; None when no type's prefix does.

  Names the type of a backend id that config no longer defines.
  """
  for backend_type, registration in _registry.items():
    prefix = registration.traits.family_prefix
    if prefix is not None and backend_id.startswith(prefix):
      return backend_type
  return None


def registered_types() -> tuple[str, ...]:
  """Every registered type, in registration order."""
  return tuple(_registry)


def same_continuation_domain(current_id: str, target_id: str, cfg: CharlieBotConfig) -> bool:
  """True when the two configured backend ids share one continuation domain.

  False when either id is not a configured option: an unknown id has no domain to share.
  """
  current = cfg.get_backend_option(current_id)
  target = cfg.get_backend_option(target_id)
  if current is None or target is None:
    return False
  current_domain = lifecycle_for(current).continuation_domain(current, cfg)
  target_domain = lifecycle_for(target).continuation_domain(target, cfg)
  return current_domain == target_domain
