"""The sentence a registry in this package uses to refuse a name it already holds.

``wiring``, ``page_render``, ``scheduled_handlers``, ``sidebar_contributions`` and ``turn_contributions``
each hold registrations by name. A second registration of one name raises this ValueError in every one of
them, so the refusal reads the same whichever registry caught it.
"""


def already_registered(kind: str, name: str, owner: object) -> ValueError:
  """The ValueError for a second registration of *name*, where *kind* names the registry and *owner* is the
  registration that already holds the name."""
  return ValueError(f"{kind} {name!r} is already registered by {owner}")
