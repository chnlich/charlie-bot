"""The deferred build_backend loader shared by the carriers that build one backend.

The registry stack (src.agents.backends.registry and every backend module it
imports, ~35 ms of the M99 server import floor in docs/perf_baseline.md) must
stay off the server import chain, so each carrier resolves the builder at its
one build site; this module imports the registry only inside that call.
"""

from typing import Any


def load_build_backend(namespace: dict[str, Any]) -> Any:
  """Bind the registry's ``build_backend`` into *namespace* on first use and return it.

  Each carrier passes its own ``globals()``: an existing binding — a test's
  stand-in — is returned untouched, so the carrier's module attribute (e.g.
  ``src.core.recap.build_backend``) stays the tests' patch target, exactly as
  ``load_requests`` does for requests.
  """
  bound = namespace.get("build_backend")
  if bound is not None:
    return bound
  from src.agents.backends.registry import build_backend

  namespace["build_backend"] = build_backend
  return build_backend


def build_backend_module_getattr(name: str, module_name: str, namespace: dict[str, Any]) -> Any:
  """Body of a carrier module's PEP 562 ``__getattr__`` that serves the deferred build_backend.

  Each carrier defines ``def __getattr__(name): return
  build_backend_module_getattr(name, __name__, globals())``. The
  module-attribute route (e.g. ``src.core.recap.build_backend``) is the tests'
  patch target; ``load_build_backend`` binds the same object as a module global
  on first use, so later attribute reads resolve directly. Any other name
  raises AttributeError, as PEP 562 requires.
  """
  if name != "build_backend":
    raise AttributeError(f"module {module_name!r} has no attribute {name!r}")
  return load_build_backend(namespace)
