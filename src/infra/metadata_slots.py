"""The registry of top-level metadata keys that a package owns.

``SessionMetadata`` and ``ThreadMetadata`` declare only the fields the core runtime owns. A package that
needs keys of its own in ``metadata.json`` registers a pydantic model whose fields are those keys
(``register_metadata_fields`` in ``src/infra/metadata_slot_registration.py``) and reads and writes them
through this module (``fields_of``, ``set_fields``). Deleting the package then leaves no field name in infra.

Vocabulary:

- An *owner* is the package that registers keys, named by a string such as "claude_code".
- ``on`` names the metadata file: "session" is ``SessionMetadata``, "thread" is ``ThreadMetadata``.
  One owner registers at most one model per file.
- A *slot* is one registration: the owner, its model and where its keys sit in the saved file.

The keys live where they always did. A metadata model keeps every key it does not declare
(``extra="allow"``) and writes it back, and a loaded file with a key that no owner registers is
not an error: a deleted package leaves inert keys. The saved file lists a slot's keys after the
declared field named by ``after``, and lists a key that the file lacks with its default, which is
where and how a declared field was saved.

A registration stores strings only. The first call of any public function below resolves every registration
that is still pending: it imports the model, makes the checks that need it and builds the slot. A
registration made after that resolves on the next call.

``models.py`` (the module that defines the two metadata models) imports this module and calls
``provide_metadata_models`` once, after it defines them. This module imports nothing from ``models.py``: every public
function receives a metadata object or runs inside a metadata model's own method, so the models are loaded when it runs.
"""

from __future__ import annotations

import copy
import dataclasses
import threading
from typing import Any

from pydantic import BaseModel, SerializationInfo, ValidationError

from src.infra import metadata_slot_registration
from src.infra.deferred import import_attr
from src.infra.metadata_slot_registration import FILES, ON_SESSION, ON_THREAD


@dataclasses.dataclass(frozen=True)
class _Slot:
  owner: str
  model: type[BaseModel]
  after: str | None
  defaults_python: dict[str, Any]
  defaults_json: dict[str, Any]

  @property
  def names(self) -> tuple[str, ...]:
    return tuple(self.model.model_fields)


# file ("session" | "thread") -> owner -> slot, in registration order
_slots: dict[str, dict[str, _Slot]] = {on: {} for on in FILES}
# How many registrations (``metadata_slot_registration.registered()``) the slots cover. A registration that fails to
# resolve stays uncounted, so every later call raises its error again.
_resolved = 0
_resolve_lock = threading.Lock()
# file ("session" | "thread") -> the metadata model, set once by ``provide_metadata_models``
_metadata_classes: dict[str, type[BaseModel]] = {}


def provide_metadata_models(session: type[BaseModel], thread: type[BaseModel]) -> None:
  """Receive ``SessionMetadata`` and ``ThreadMetadata`` from ``models.py``, which calls this once."""
  _metadata_classes[ON_SESSION] = session
  _metadata_classes[ON_THREAD] = thread


def _file_of(meta: BaseModel) -> str:
  for on, metadata in _metadata_classes.items():
    if isinstance(meta, metadata):
      return on
  raise TypeError(f"{type(meta).__name__} is neither SessionMetadata nor ThreadMetadata")


def _resolve(registration: metadata_slot_registration.Registration) -> _Slot:
  """The slot of one registration; ValueError when its model collides with a field or its ``after`` is undeclared."""
  owner, on, after = registration.owner, registration.on, registration.after
  metadata = _metadata_classes[on]
  if after is not None and after not in metadata.model_fields:
    raise ValueError(f"{after!r} is not a declared field of {metadata.__name__}")
  model_cls = import_attr(registration.model)
  for name in model_cls.model_fields:
    if name in metadata.model_fields:
      raise ValueError(f"{owner!r} field {name!r} collides with a field of {metadata.__name__}")
    for other in _slots[on].values():
      if name in other.model.model_fields:
        raise ValueError(f"{owner!r} field {name!r} collides with a field of {other.owner!r}")
  defaults = model_cls()
  return _Slot(
      owner=owner,
      model=model_cls,
      after=after,
      defaults_python=defaults.model_dump(),
      defaults_json=defaults.model_dump(mode="json"))


def _resolve_pending() -> None:
  """Resolve every registration made since the last call; with none pending this is one comparison.

  The lock lets concurrent first calls resolve each registration once: the thread that waits finds
  nothing pending.
  """
  global _resolved
  if _resolved == len(metadata_slot_registration.registered()):
    return
  with _resolve_lock:
    for registration in metadata_slot_registration.registered()[_resolved:]:
      _slots[registration.on][registration.owner] = _resolve(registration)
      _resolved += 1


def _slot(meta: BaseModel, owner: str) -> _Slot:
  on = _file_of(meta)
  slot = _slots[on].get(owner)
  if slot is None:
    raise KeyError(f"{owner!r} registered no fields on {on} metadata")
  return slot


def _held(meta: BaseModel, slot: _Slot) -> dict[str, Any]:
  extra = meta.model_extra or {}
  return {name: extra[name] for name in slot.names if name in extra}


def fields_of(meta: BaseModel, owner: str) -> BaseModel:
  """The validated view over ``owner``'s keys in ``meta``; a key the file lacks reads as its default.

  A stored value of the wrong type raises ValidationError.
  """
  _resolve_pending()
  slot = _slot(meta, owner)
  return slot.model.model_validate(_held(meta, slot))


def set_fields(meta: BaseModel, owner: str, **values: Any) -> None:
  """Validate ``values`` against ``owner``'s model, then write those keys into ``meta``.

  A name that is not a field of the owner's model raises ValueError; a value of the wrong type raises
  ValidationError. Nothing is written when either raises.
  """
  _resolve_pending()
  slot = _slot(meta, owner)
  unknown = sorted(set(values) - set(slot.names))
  if unknown:
    raise ValueError(f"{owner!r} has no fields {unknown}")
  validated = slot.model.model_validate({**_held(meta, slot), **values})
  for name in values:
    setattr(meta, name, getattr(validated, name))


def copy_fields(
    source: BaseModel,
    target: BaseModel,
    owner: str,
    *,
    names: tuple[str, ...] | None = None,
) -> None:
  """Copy all or selected fields of one registered owner from *source* to *target*."""
  values = fields_of(source, owner).model_dump()
  if names is not None:
    values = {name: values[name] for name in names}
  set_fields(target, owner, **values)


def _owned_names(on: str) -> dict[str, str]:
  return {name: owner for owner, slot in _slots[on].items() for name in slot.names}


def set_registered(meta: BaseModel, values: dict[str, Any]) -> None:
  """Write ``values``, keys of any owner registered on ``meta``'s file, through ``set_fields``, one call per owner.

  A key that no owner registers raises ValueError.
  """
  _resolve_pending()
  owned = _owned_names(_file_of(meta))
  unknown = sorted(set(values) - set(owned))
  if unknown:
    raise ValueError(f"no owner registered the keys {unknown}")
  by_owner: dict[str, dict[str, Any]] = {}
  for name, value in values.items():
    by_owner.setdefault(owned[name], {})[name] = value
  for owner, owner_values in by_owner.items():
    set_fields(meta, owner, **owner_values)


def check_registered(on: str, values: dict[str, Any]) -> None:
  """Raise ValueError when ``values`` holds a key no owner registered on the ``on`` file or a value of the wrong type."""
  _resolve_pending()
  owned = _owned_names(on)
  unknown = sorted(set(values) - set(owned))
  if unknown:
    raise ValueError(f"unknown keys {unknown}")
  for owner, slot in _slots[on].items():
    held = {name: values[name] for name in slot.names if name in values}
    try:
      slot.model.model_validate(held)
    except ValidationError as exc:
      raise ValueError(f"invalid value for the keys of {owner!r}: {exc}") from exc


def arrange(on: str, declared: dict[str, Any], data: dict[str, Any], info: SerializationInfo) -> dict[str, Any]:
  """The saved or served form of one metadata dump: the dump's keys in file order with every slot key present.

  ``declared`` is the metadata model's field table and ``data`` the dump as pydantic produced it: declared
  fields in declaration order, then the extra keys in the order the file listed them. Slot keys leave the
  extras and take their place after the slot's ``after`` field, a missing one filled with its default; the
  unregistered extras stay last. Keys that ``info`` excludes stay out.
  """
  _resolve_pending()
  slots = _slots[on]
  if not slots:
    return data
  owned = _owned_names(on)
  as_json = info.mode == "json"
  out: dict[str, Any] = {}
  placed: set[str] = set()

  def place(owner: str) -> None:
    slot = slots[owner]
    defaults = slot.defaults_json if as_json else slot.defaults_python
    for name in slot.names:
      if info.include is not None and name not in info.include:
        continue
      if info.exclude is not None and name in info.exclude:
        continue
      out[name] = data[name] if name in data else copy.deepcopy(defaults[name])
    placed.add(owner)

  for key, value in data.items():
    if key in declared:
      out[key] = value
      for owner, slot in slots.items():
        if slot.after == key:
          place(owner)
  for owner in slots:
    if owner not in placed:
      place(owner)
  for key, value in data.items():
    if key not in declared and key not in owned:
      out[key] = value
  return out
