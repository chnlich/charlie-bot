"""The registration half of the backend type table (``src/runtime/hooks/backend_types.py`` holds the vocabulary).

A backend package calls ``register_backend_type`` from its ``register()`` with the traits of its type and the
"module:attr" strings of its option model, factory and lifecycle. This module imports no backend module, no
``dataclasses`` and no pydantic, so registering costs no heavy import. ``backend_types`` builds backends and
reads the table.
"""

from collections.abc import Mapping
from typing import NamedTuple

from src.infra import config_registry

RESUME_CLI_FLAG = "cli_flag"
RESUME_NATIVE_ID = "native_id"
_RESUME_STYLES = (RESUME_CLI_FLAG, RESUME_NATIVE_ID)


class BackendTraits(NamedTuple):
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


class Registration(NamedTuple):
  factory: str
  traits: BackendTraits
  lifecycle: str | None
  translate_fallback: bool


_registry: dict[str, Registration] = {}


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
  raises ValueError, and so does a resume style other than ``RESUME_CLI_FLAG`` and ``RESUME_NATIVE_ID``.
  """
  if backend_type in _registry:
    raise ValueError(f"backend type {backend_type!r} is already registered")
  if traits.resume not in _RESUME_STYLES:
    raise ValueError(f"resume style {traits.resume!r} is not one of {_RESUME_STYLES}")
  if translate_fallback and any(registration.translate_fallback for registration in _registry.values()):
    raise ValueError(f"backend type {backend_type!r} cannot serve the translate fallback: another type does")
  config_registry.register_option_model(backend_type, options)
  _registry[backend_type] = Registration(
      factory=factory, traits=traits, lifecycle=lifecycle, translate_fallback=translate_fallback)


def registration_of(backend_type: str) -> Registration:
  """The registration of ``backend_type``; ValueError when the type is unregistered."""
  registration = _registry.get(backend_type)
  if registration is None:
    raise ValueError(f"Unknown backend type: {backend_type}")
  return registration


def registrations() -> Mapping[str, Registration]:
  """Every registration by backend type, in registration order. Callers read it and never change it."""
  return _registry
