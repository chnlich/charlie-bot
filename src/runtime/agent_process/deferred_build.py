"""The build_backend loader shared by the carriers that build one backend.

A carrier binds ``build_backend`` into its own namespace at its one build site, on first use. The backend
type table (src/runtime/hooks/backend_types.py) imports a backend module only when its factory runs.
"""

from typing import Any

from src.runtime.hooks import backend_types


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

  namespace["build_backend"] = backend_types.build_backend
  return backend_types.build_backend
