"""The sentence a registry uses to refuse a name it already holds.

``config_registry`` here and ``wiring``, ``page_render``, ``scheduled_handlers``, ``backend_type_registration``,
``backend_lifecycle_registration``, ``usage_source_registration``, ``sequence_controllers``,
``sidebar_contributions`` and ``turn_contributions`` under ``src/runtime/hooks`` each hold registrations by name. A
second registration of one name raises this ValueError in every one of them, so the refusal reads the same whichever
registry caught it.
"""


def already_registered(kind: str, name: str, owner: object | None = None) -> ValueError:
  """The ValueError for a second registration of *name*, where *kind* names the registry and heads the sentence.

  *owner*, when given, is the registration that already holds the name and the sentence names it.
  """
  sentence = f"{kind} {name!r} is already registered"
  return ValueError(sentence if owner is None else f"{sentence} by {owner}")
