"""The environment variables that carry a CharlieBot identity or credential into a process.

An isolated trial (a preview server on a seeded home and the live-trial harnesses) must not inherit any of them
from the process that starts it. The runtime owns two names; a package that injects or reads a
credential of its own registers that name from its ``register()`` (``register_identity_env_var``).
Read the full list with ``inherited_identity_env_vars()`` after ``register_all()`` has run.
"""

from src.infra import constants

_registered: list[str] = []


def register_identity_env_var(name: str) -> None:
  """Add ``name`` to the variables an isolated trial must not inherit. A second registration of one name raises ValueError."""
  if name in _registered:
    raise ValueError(f"identity env var {name!r} is already registered")
  _registered.append(name)


def inherited_identity_env_vars() -> tuple[str, ...]:
  """The runtime's own names, then every registered name in registration order."""
  return (constants.SESSION_ID_ENV_VAR, constants.RUN_TOKEN_ENV, *_registered)
