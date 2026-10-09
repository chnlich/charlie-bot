"""The backend type table: how the runtime builds and judges each backend type.

A backend package registers its type with ``src/runtime/hooks/backend_type_registration.py`` from its
``register()`` function (the packages listed in ``src/app/registrations.py``). The runtime builds backends,
reads resume and relay behavior, and reads context limits through this module and imports no backend module.

Vocabulary:

- A *type* is a ``BackendOption.type`` string such as "codex".
- The *option model* of a type is the pydantic model of its ``backends.options[]`` entry
  (``src/infra/config_registry.py`` holds it).
- The *factory* of a type builds its ``AgentBackend`` from one option.
- The *traits* of a type are the behaviors that runtime code reads by type, in ``BackendTraits``
  (``backend_type_registration.py``).
- The *lifecycle* of a type is its ``BackendLifecycle`` (``backend_lifecycle.py``).

The option model, the factory and the lifecycle are "module:attr" strings. They import on first
use, so a registration costs no backend import.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Protocol

from src.infra.deferred import import_attr
from src.runtime.hooks import backend_lifecycle, backend_type_registration

if TYPE_CHECKING:
  from src.infra.config import CharlieBotConfig
  from src.infra.models import BackendOption, SessionMetadata
  from src.runtime.hooks import backend_lifecycle


class BuiltBackend(Protocol):
  """The backend object a factory returns.

  The type table hands it to the caller and reads no member of it, so the protocol lists none.
  """


_lifecycles: dict[str, backend_lifecycle.BackendLifecycle] = {}
_DEFAULT_LIFECYCLE: backend_lifecycle.BackendLifecycle | None = None


def build_backend(option: BackendOption, cfg: CharlieBotConfig, **launch_kwargs: Any) -> BuiltBackend:
  """Instantiate the backend of ``option.type``.

  ``launch_kwargs`` go to the factory, which forwards them to the backend constructor (for example
  extra_flags, buffer_limit, on_spawn). Raises ValueError when the type is unregistered or
  required config is missing.
  """
  return import_attr(backend_type_registration.registration_of(option.type).factory)(option, cfg, **launch_kwargs)


def build_translate_fallback(cfg: CharlieBotConfig, **launch_kwargs: Any) -> BuiltBackend:
  """Instantiate the backend of the type that serves the binary-free translate fallback."""
  for registration in backend_type_registration.registrations().values():
    if registration.translate_fallback:
      return import_attr(registration.factory)(None, cfg, **launch_kwargs)
  raise ValueError("no backend type serves the translate fallback")


def traits_for(backend_type: str) -> backend_type_registration.BackendTraits:
  """The traits of ``backend_type``; ValueError when the type is unregistered."""
  return backend_type_registration.registration_of(backend_type).traits


def _lifecycle_at(path: str | None) -> backend_lifecycle.BackendLifecycle:
  """The lifecycle instance a registered path names; the default lifecycle for None."""
  global _DEFAULT_LIFECYCLE

  if path is None:
    if _DEFAULT_LIFECYCLE is None:
      _DEFAULT_LIFECYCLE = backend_lifecycle.BackendLifecycle()
    return _DEFAULT_LIFECYCLE
  if path not in _lifecycles:
    _lifecycles[path] = import_attr(path)()
  return _lifecycles[path]


def lifecycle_for(option: BackendOption) -> backend_lifecycle.BackendLifecycle:
  """The lifecycle of ``option.type``; the default lifecycle when the type registered none."""
  return _lifecycle_at(backend_type_registration.registration_of(option.type).lifecycle)


def lifecycles() -> tuple[backend_lifecycle.BackendLifecycle, ...]:
  """Every registered lifecycle, each once, in registration order; types without one are left out."""
  registrations = backend_type_registration.registrations().values()
  paths = dict.fromkeys(r.lifecycle for r in registrations if r.lifecycle is not None)
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
  for backend_type, registration in backend_type_registration.registrations().items():
    prefix = registration.traits.family_prefix
    if prefix is not None and backend_id.startswith(prefix):
      return backend_type
  return None


def registered_types() -> tuple[str, ...]:
  """Every registered type, in registration order."""
  return tuple(backend_type_registration.registrations())


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
