"""The typed per-type backend entries: one class per ``type``, discriminated union, retired fields rejected."""

import pytest
from pydantic import ValidationError

from src.core.models import (
    BACKEND_CLASSES,
    BACKEND_OPTION_ADAPTER,
    MODEL_OPTIONAL_ROUTING_BACKEND_TYPES,
    BackendBase,
    BackendType,
    CharlieCodeBackend,
)

# Field names the flat BackendOption carried that exist on no typed class; extra='forbid'
# must reject them on every entry.
RETIRED_FIELDS = frozenset(
    {"aliases", "claude_config_dir", "codex_home", "api_key", "api_key_env", "opencode_proxy_url"})


def backend_type(cls: type[BackendBase]) -> BackendType:
  """The ``type`` value the class pins."""
  return cls.model_fields["type"].default


def minimal_payload(cls: type[BackendBase]) -> dict:
  """The smallest mapping the class accepts: id, label, type, plus each required field."""
  payload: dict = {"id": f"test-{backend_type(cls).value}", "label": "Test backend", "type": backend_type(cls)}
  if backend_type(cls) not in MODEL_OPTIONAL_ROUTING_BACKEND_TYPES:
    payload["model"] = "test-model"
  for name in ("credential", "api_base"):
    field = cls.model_fields.get(name)
    if field is not None and field.is_required():
      payload[name] = "https://example.invalid/v1" if name == "api_base" else f"test-{name}"
  return payload


def ids(cls: type[BackendBase]) -> str:
  return backend_type(cls).value


@pytest.mark.parametrize("cls", BACKEND_CLASSES, ids=ids)
def test_extra_forbid(cls: type[BackendBase]) -> None:
  assert cls.model_config["extra"] == "forbid"


@pytest.mark.parametrize("cls", BACKEND_CLASSES, ids=ids)
def test_minimal_payload_validates_to_its_class(cls: type[BackendBase]) -> None:
  assert type(BACKEND_OPTION_ADAPTER.validate_python(minimal_payload(cls))) is cls


@pytest.mark.parametrize("cls", BACKEND_CLASSES, ids=ids)
def test_foreign_and_retired_fields_rejected(cls: type[BackendBase]) -> None:
  every_field = {name for other in BACKEND_CLASSES for name in other.model_fields}
  rejected = (every_field - set(cls.model_fields)) | RETIRED_FIELDS
  for name in sorted(rejected):
    with pytest.raises(ValidationError, match=name):
      BACKEND_OPTION_ADAPTER.validate_python({**minimal_payload(cls), name: "value"})


@pytest.mark.parametrize("cls", BACKEND_CLASSES, ids=ids)
def test_model_optional_only_for_routing_types(cls: type[BackendBase]) -> None:
  payload = minimal_payload(cls)
  payload.pop("model", None)
  if backend_type(cls) in MODEL_OPTIONAL_ROUTING_BACKEND_TYPES:
    assert type(BACKEND_OPTION_ADAPTER.validate_python(payload)) is cls
  else:
    with pytest.raises(ValidationError):
      BACKEND_OPTION_ADAPTER.validate_python(payload)


def test_charlie_code_image_input_accepted_and_defaults_false() -> None:
  """image_input is a charlie-code-only option: accepted on its entries, defaulting to false."""
  payload = minimal_payload(CharlieCodeBackend)
  assert BACKEND_OPTION_ADAPTER.validate_python(payload).image_input is False
  accepted = BACKEND_OPTION_ADAPTER.validate_python({**payload, "image_input": True})
  assert accepted.image_input is True
  assert type(accepted) is CharlieCodeBackend
