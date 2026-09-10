"""Schema and loader gates for the sectioned CharlieBotConfig."""

import re
from pathlib import Path
from typing import get_args, get_origin

import pytest
import yaml
from pydantic import BaseModel

from src.core import config as config_module
from src.core.config import CHARLIEBOT_HOME_ENV, CREDENTIALS_PREFIX, LEGACY_KEYS, CharlieBotConfig
from src.core.models import BACKEND_CLASSES

FIXTURE_PATH = Path(__file__).parent / "fixtures" / "config_legacy.yaml"

SECTION_NAMES = {
    "ServerConfig",
    "PathsConfig",
    "BackendsConfig",
    "AccountsConfig",
    "VoiceConfig",
    "CodeServerConfig",
    "UiConfig",
    "SlackConfig",
    "PublishConfig",
    "TelegramConfig",
}


def _models_in(annotation: object) -> list[type[BaseModel]]:
  """Pydantic models inside an annotation: a bare model class, or models nested in list/union wrappers."""
  if isinstance(annotation, type) and issubclass(annotation, BaseModel):
    return [annotation]
  found: list[type[BaseModel]] = []
  for arg in get_args(annotation):
    found.extend(_models_in(arg))
  return found


def _reachable_models(root: type[BaseModel]) -> set[type[BaseModel]]:
  """Every pydantic model reachable from *root*'s field annotations, transitively."""
  seen: set[type[BaseModel]] = set()
  stack = [root]
  while stack:
    model = stack.pop()
    if model in seen:
      continue
    seen.add(model)
    for field in model.model_fields.values():
      stack.extend(_models_in(field.annotation))
  return seen


def test_every_reachable_model_forbids_extra_fields():
  models = _reachable_models(CharlieBotConfig) | set(BACKEND_CLASSES)
  assert SECTION_NAMES <= {model.__name__ for model in models}  # the walk reached the sections
  assert {model.__name__ for model in BACKEND_CLASSES} <= {model.__name__ for model in models}
  for model in sorted(models, key=lambda m: m.__name__):
    assert model.model_config.get("extra") == "forbid", model.__name__


def test_load_config_names_every_legacy_key_in_the_file(tmp_path, monkeypatch):
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_bytes(FIXTURE_PATH.read_bytes())
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  with pytest.raises(ValueError) as excinfo:
    config_module.load_config()
  message = str(excinfo.value)
  assert str(home / "config.yaml") in message.splitlines()[0]
  named = dict(re.findall(r"^  (.+) -> (.+)$", message, re.M))
  expected = set(yaml.safe_load(FIXTURE_PATH.read_text(encoding="utf-8"))) & set(LEGACY_KEYS)
  assert set(named) == expected
  for key, location in named.items():
    assert location == LEGACY_KEYS[key]


def test_legacy_table_matches_the_model_tree():
  for old_key, location in LEGACY_KEYS.items():
    assert old_key not in CharlieBotConfig.model_fields, old_key
    # credentials-prefixed items move to credentials.yaml (not this model tree);
    # removed items have no successor.
    if old_key.startswith(CREDENTIALS_PREFIX) or location.startswith("removed"):
      continue
    model = CharlieBotConfig
    for segment in location.split("."):
      assert segment in model.model_fields, (old_key, location, segment)
      annotation = model.model_fields[segment].annotation
      if get_origin(annotation) is list:
        break  # stop at the first list field: the value migrates onto the list itself
      if isinstance(annotation, type) and issubclass(annotation, BaseModel):
        model = annotation


@pytest.mark.parametrize("fragment_name", ["x.yaml", "cron.yaml"])
def test_config_d_fragments_are_rejected(tmp_path, monkeypatch, fragment_name):
  home = tmp_path / "home"
  (home / "config.d").mkdir(parents=True)
  (home / "config.yaml").write_text("server:\n  host: 127.0.0.1\n", encoding="utf-8")
  (home / "config.d" / fragment_name).write_text("voice:\n  engine: sherpa\n", encoding="utf-8")
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  with pytest.raises(ValueError) as excinfo:
    config_module.load_config()
  assert f"config.d/{fragment_name}" in str(excinfo.value)


def test_config_d_cron_d_files_are_not_fragments(tmp_path, monkeypatch):
  home = tmp_path / "home"
  (home / "config.d" / "cron.d").mkdir(parents=True)
  (home / "config.d" / "cron.d" / "nightly.yaml").write_text("name: nightly\n", encoding="utf-8")
  (home / "config.yaml").write_text("server:\n  port: 2001\n", encoding="utf-8")
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  assert config_module.load_config().server.port == 2001


def test_backend_entry_unknown_field_names_id_type_and_field(tmp_path, monkeypatch):
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_text(
      "backends:\n"
      "  options:\n"
      "    - id: test-backend\n"
      "      type: cc-claude\n"
      "      label: TB\n"
      "      model: some-model\n"
      "      bogus_field: 1\n",
      encoding="utf-8")
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  with pytest.raises(ValueError) as excinfo:
    config_module.load_config()
  assert str(excinfo.value) == "backend entry 'test-backend' (type cc-claude) has unknown field 'bogus_field'"
