"""_backend_dispatch reads cli_binary only off the option classes that declare it.

Every BACKEND_CLASSES member must dispatch with its option's type and a cli_binary
of getattr(option, 'cli_binary', None) — the declared value for cc-claude/tui-cli,
None for the rest — instead of the AttributeError a bare read raises on the
extra='forbid' models (that 500 is what blanked the workers-panel events).
"""

from pathlib import Path

import pytest

from src.api.threads import _backend_dispatch
from src.core.config import CharlieBotConfig
from src.core.models import (
    BACKEND_CLASSES,
    MODEL_OPTIONAL_ROUTING_BACKEND_TYPES,
    BackendBase,
    BackendType,
    CcClaudeBackend,
    ThreadMetadata,
    TuiCliBackend,
)

# Inputs, not expectations: values for the two option classes that declare
# cli_binary (claude-sub is the production spelling). Every expectation below is
# derived from the constructed option.
_CLI_BINARY_BY_CLASS: dict[type[BackendBase], str] = {
    CcClaudeBackend: "claude-sub",
    TuiCliBackend: "tui-attach",
}


def _backend_type(cls: type[BackendBase]) -> BackendType:
  return cls.model_fields["type"].default


def _option_kwargs(cls: type[BackendBase]) -> dict:
  """Constructor kwargs for one instance of cls, derived from its model_fields.

  id/label get test values, model is set unless the type opts out (require_model
  enforces that in a validator, not through is_required()), and each remaining
  required field is filled from its annotation. A new required non-str field
  fails here instead of the test faking a value it cannot derive.
  """
  kwargs: dict = {"id": f"test-{_backend_type(cls).value}", "label": "Test backend"}
  if _backend_type(cls) not in MODEL_OPTIONAL_ROUTING_BACKEND_TYPES:
    kwargs["model"] = "test-model"
  for name, field in cls.model_fields.items():
    if name in kwargs or not field.is_required():
      continue
    if field.annotation is not str:
      raise AssertionError(f"{cls.__name__}.{name}: no derived value for required {field.annotation!r}")
    kwargs[name] = f"test-{name}"
  return kwargs


@pytest.mark.parametrize("cls", BACKEND_CLASSES, ids=lambda cls: _backend_type(cls).value)
def test_backend_dispatch_reads_only_declared_cli_binary(cls: type[BackendBase], tmp_path: Path) -> None:
  kwargs = _option_kwargs(cls)
  if cls in _CLI_BINARY_BY_CLASS:
    kwargs["cli_binary"] = _CLI_BINARY_BY_CLASS[cls]
  option = cls(**kwargs)

  thread = ThreadMetadata(session_id="sess-1", description="task", backend=option.id)
  cfg = CharlieBotConfig(charliebot_home=tmp_path / "home", backends={"options": [option]})

  dispatch = _backend_dispatch(thread, cfg)

  assert dispatch is not None
  assert dispatch.type == option.type
  assert dispatch.cli_binary == getattr(option, "cli_binary", None)
