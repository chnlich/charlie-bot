"""Schema and loader gates for the sectioned CharlieBotConfig."""

import asyncio
import json
import os
import re
from pathlib import Path
from typing import get_args, get_origin

import pytest
import yaml
from pydantic import BaseModel

from src.core import config as config_module
from src.core.config import CHARLIEBOT_HOME_ENV, CREDENTIALS_PREFIX, LEGACY_KEYS, CharlieBotConfig
from src.core.init_seed import init_charliebot_home
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


def _credentials_home(tmp_path, monkeypatch) -> Path:
  """A temp CHARLIEBOT_HOME with a minimal valid sectioned config.yaml; returns the home path."""
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_text("server:\n  port: 2001\n", encoding="utf-8")
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  return home


def test_load_without_credentials_file_gives_empty_sections(tmp_path, monkeypatch):
  home = _credentials_home(tmp_path, monkeypatch)
  assert config_module.load_config().server.port == 2001
  credentials = config_module.load_credentials()
  assert credentials.sections == {}
  assert credentials.path == home / "credentials.yaml"


def test_credentials_stay_out_of_config_and_get_returns_each_sentinel(tmp_path, monkeypatch):
  home = _credentials_home(tmp_path, monkeypatch)
  sections = {
      "alpha": {"token": "sentinel-alpha-token", "secret": "sentinel-alpha-secret"},
      "beta": {"token": "sentinel-beta-token", "secret": "sentinel-beta-secret"},
  }
  (home / "credentials.yaml").write_text(yaml.safe_dump(sections), encoding="utf-8")
  dumped = json.dumps(config_module.load_config().model_dump(mode="json"))
  credentials = config_module.load_credentials()
  for section, keys in sections.items():
    for key, sentinel in keys.items():
      assert sentinel not in dumped
      assert credentials.get(section, key) == sentinel
  with pytest.raises(ValueError) as excinfo:
    credentials.require("alpha", "missing_key")
  assert str(excinfo.value) == f"credentials.alpha.missing_key is not set in {home / 'credentials.yaml'}"


@pytest.mark.parametrize(
    "body, fragment",
    [
        ("- one\n- two\n", "credentials must be a mapping"),
        ("alpha: scalar\n", "credentials.alpha"),
        ("alpha:\n  key: [1, 2]\n", "credentials.alpha.key"),
    ],
)
def test_credentials_shape_errors_name_the_offending_depth(tmp_path, monkeypatch, body, fragment):
  home = _credentials_home(tmp_path, monkeypatch)
  (home / "credentials.yaml").write_text(body, encoding="utf-8")
  with pytest.raises(ValueError) as excinfo:
    config_module.load_credentials()
  assert fragment in str(excinfo.value)


def test_get_credentials_caches_until_the_file_changes(tmp_path, monkeypatch):
  home = _credentials_home(tmp_path, monkeypatch)
  cred_path = home / "credentials.yaml"
  cred_path.write_text("alpha:\n  key: one\n", encoding="utf-8")
  first = config_module.get_credentials()
  assert first.get("alpha", "key") == "one"
  assert config_module.get_credentials() is first
  cred_path.write_text("alpha:\n  key: two\n", encoding="utf-8")
  st = os.stat(cred_path)
  os.utime(cred_path, (st.st_atime, st.st_mtime + 10))
  second = config_module.get_credentials()
  assert second is not first
  assert second.get("alpha", "key") == "two"


def test_credentials_example_covers_every_credentials_legacy_key():
  example_path = Path(__file__).resolve().parents[1] / "configs" / "credentials.example.yaml"
  raw_lines = example_path.read_text(encoding="utf-8").splitlines()
  stripped = "\n".join(line[2:] if line.startswith("# ") else line for line in raw_lines)
  example = yaml.safe_load(stripped)
  assert isinstance(example, dict)
  for old_key, location in LEGACY_KEYS.items():
    if not old_key.startswith(CREDENTIALS_PREFIX):
      continue
    section, key = location.split(".", 1)
    assert section in example, old_key
    assert key in example[section], old_key


EXAMPLE_PATH = Path(__file__).resolve().parents[1] / "configs" / "config.example.yaml"

STARTER_BACKEND_IDS = ["claude-fable-5", "claude-opus-5", "claude-sonnet-5", "claude-tui"]


def test_example_config_loads_to_the_model_default(tmp_path, monkeypatch):
  """The shipped example is the default config: loading it equals constructing
  CharlieBotConfig, modulo backends.options (the example ships the four starter
  entries where the model default is empty)."""
  home = tmp_path / "home"
  home.mkdir()
  (home / "config.yaml").write_bytes(EXAMPLE_PATH.read_bytes())
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  loaded = config_module.load_config()
  loaded_dump = loaded.model_dump()
  default_dump = CharlieBotConfig(charliebot_home=home).model_dump()
  loaded_dump["backends"]["options"] = None
  default_dump["backends"]["options"] = None
  assert loaded_dump == default_dump
  assert [option.id for option in loaded.backends.options] == STARTER_BACKEND_IDS


def test_example_config_is_block_style():
  """No inline mappings and no non-empty inline lists outside comment lines."""
  for line in EXAMPLE_PATH.read_text(encoding="utf-8").splitlines():
    if line.strip().startswith("#"):
      continue
    assert not re.search(r"\{|\[[^\]]", line), line


def test_init_charliebot_home_seeds_config_and_credentials(tmp_path, monkeypatch):
  """A fresh home gets config.yaml byte-equal to the example and credentials.yaml
  from the repo template, owner-readable only, loading as empty sections."""
  home = tmp_path / "home"
  home.mkdir()
  monkeypatch.setenv(CHARLIEBOT_HOME_ENV, str(home))
  fake_cfg = CharlieBotConfig(charliebot_home=home)
  monkeypatch.setattr("src.core.init_seed.get_config", lambda: fake_cfg)
  asyncio.run(init_charliebot_home())
  credentials_path = home / "credentials.yaml"
  assert credentials_path.exists()
  assert credentials_path.stat().st_mode & 0o777 == 0o600
  assert config_module.load_credentials().sections == {}
  assert (home / "config.yaml").read_bytes() == EXAMPLE_PATH.read_bytes()
