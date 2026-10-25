"""The PEP 562 ``__getattr__`` body shared by the deferred-import loaders.

Deferred imports (``requests``, ``build_backend``, ``base_one_shot_text`` and the
``src.infra.backend_models`` re-exports) bind through a per-consumer loader; the
consumer's ``__getattr__`` is what fires that loader
on a module-attribute read, so a test's patch target (e.g.
``src.runtime.worker.build_backend``) resolves without importing the real symbol
at module import. The match-or-AttributeError rule lives here, once; the
globals-first loader the master-turn chain's consumers build theirs from
lives here too. A "module:attr" spec is split here as well, so its grammar has
one home.
"""

import importlib
from collections.abc import Callable
from typing import Any


def deferred_module_getattr(
    name: str,
    module_name: str,
    namespace: dict[str, Any],
    target: str,
    load: Callable[[dict[str, Any]], Any],
) -> Any:
  """Serve *target* through its loader on first attribute read.

  Each consumer module defines ``def __getattr__(name): return
  deferred_module_getattr(name, __name__, globals(), "target", loader)``. The
  module-attribute route (e.g. ``<consumer module>.requests``) stays the
  tests' patch target; the loader binds the same object as a module global on
  first use, so the consumer's own bare-name reads resolve directly. Any other
  name raises AttributeError, as PEP 562 requires.
  """
  if name != target:
    raise AttributeError(f"module {module_name!r} has no attribute {name!r}")
  return load(namespace)


def deferred_import_loader(attr: str, module_path: str) -> Callable[[dict[str, Any]], Any]:
  """Build the loader binding *module_path*'s attribute *attr* into a namespace on first use.

  The loader returns an existing binding untouched — a test's stand-in on the
  consumer's module-attribute patch target — so the patched module attribute
  stays the seam the consumer's bare-name reads resolve through. Pair it with
  :func:`deferred_module_getattr` in the consumer's ``__getattr__``; direct
  call sites pass the consumer's ``globals()``.
  """

  def load(namespace: dict[str, Any]) -> Any:
    bound = namespace.get(attr)
    if bound is not None:
      return bound
    value = getattr(importlib.import_module(module_path), attr)
    namespace[attr] = value
    return value

  return load


def split_attr(path: str) -> tuple[str, str] | None:
  """The module name and attribute that a "module:attr" string names; None when *path* is not one.

  A spec has a non-empty module name and a non-empty attribute on either side of one colon. A caller
  that refuses a bad spec builds its own sentence, so the refusal names what the caller registers.
  """
  module_name, separator, attr = path.partition(":")
  if not separator or not module_name or not attr:
    return None
  return module_name, attr


def import_attr(path: str) -> Any:
  """The attribute that a "module:attr" string names; the module imports on this call."""
  spec = split_attr(path)
  if spec is None:
    raise ValueError(f"{path!r} is not a 'module:attr' string")
  module_name, attr = spec
  return getattr(importlib.import_module(module_name), attr)
