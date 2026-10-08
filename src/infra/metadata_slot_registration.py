"""The registration half of the metadata slot registry (``src/infra/metadata_slots.py`` holds the vocabulary).

A package calls ``register_metadata_fields`` from its ``register()`` to name the top-level keys it owns in a
metadata file. A registration stores four strings: the owner, the "module:Class" string of the model, the file
(``on``) and ``after``. It imports no pydantic, no ``src.infra.models`` and no registered model, so registering
costs no import. ``metadata_slots`` imports the models and checks them on its first call.
"""

from typing import NamedTuple

ON_SESSION = "session"
ON_THREAD = "thread"
FILES = (ON_SESSION, ON_THREAD)


class Registration(NamedTuple):
  """One registration as the package gave it; ``model`` is a "module:Class" string."""
  owner: str
  model: str
  on: str
  after: str | None


# Registration order. Entries are only appended, so a count says how many a reader has seen.
_registered: list[Registration] = []


def register_metadata_fields(owner: str, model: str, *, on: str = ON_SESSION, after: str | None = None) -> None:
  """Register ``model``, a "module:Class" string, as the top-level keys that ``owner`` holds in the ``on`` file.

  ``model`` is a pydantic model whose fields are those keys, each with a default. ``after`` names the declared
  field of the metadata model whose position the keys take in the saved file; None puts them after the declared
  fields. A second registration of one owner on one file, a ``model`` that is not a "module:Class" string and
  an unknown ``on`` raise ValueError here. The checks that need the models raise ValueError on the first call of
  a ``metadata_slots`` function: a field name that collides with the metadata model's own field or with another
  owner's field, and an ``after`` that the metadata model does not declare.
  """
  if on not in FILES:
    raise ValueError(f"on must be one of {FILES}, got {on!r}")
  module_name, separator, attr = model.partition(":")
  if not separator or not module_name or not attr:
    raise ValueError(f"{model!r} is not a 'module:Class' string")
  if any(registration.owner == owner and registration.on == on for registration in _registered):
    raise ValueError(f"{owner!r} already registered fields on {on} metadata")
  _registered.append(Registration(owner=owner, model=model, on=on, after=after))


def registered() -> list[Registration]:
  """Every registration, in registration order. The list is live: callers read it and never change it."""
  return _registered
