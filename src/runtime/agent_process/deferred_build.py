"""The deferred build_backend loader shared by the carriers that build one backend.

The backend type table (src/runtime/hooks/backend_types.py) imports a backend module only when its
factory runs, so a carrier that binds ``build_backend`` at its one build site keeps the type table
off its own import chain; this module imports the table only inside that call.
"""

from typing import Any


def load_build_backend(namespace: dict[str, Any]) -> Any:
  """Bind the type table's ``build_backend`` into *namespace* on first use and return it.

  Each carrier passes its own ``globals()``: an existing binding — a test's
  stand-in — is returned untouched, so the carrier's module attribute (e.g.
  ``src.runtime.worker.build_backend``) stays the tests' patch target, exactly as
  ``load_requests`` does for requests.
  """
  bound = namespace.get("build_backend")
  if bound is not None:
    return bound
  from src.runtime.hooks import backend_types

  namespace["build_backend"] = backend_types.build_backend
  return backend_types.build_backend
