"""The yaml_utils loader contract: libyaml's C safe pair, its parity with the
pure-Python safe pair, and the load/save behaviors every caller relies on."""

import pytest
import yaml

from src.core import yaml_utils
from src.core.yaml_utils import load_yaml, load_yaml_text, save_yaml


def test_safe_pair_binds_libyaml():
  assert yaml_utils._SAFE_LOADER is yaml.CSafeLoader
  assert yaml_utils._SAFE_DUMPER is yaml.CSafeDumper


def test_load_yaml_missing_file_returns_default(tmp_path):
  assert load_yaml(tmp_path / "absent.yaml", default={"a": 1}) == {"a": 1}


def test_load_yaml_empty_document_returns_default(tmp_path):
  path = tmp_path / "empty.yaml"
  path.write_text("", encoding="utf-8")
  assert load_yaml(path, default=[]) == []


def test_load_yaml_text_empty_document_returns_default():
  assert load_yaml_text("", default={}) == {}


def test_parsed_output_matches_safe_load():
  raw = "a: 1\nb: [1, 2, {c: d}]\ne: 'quoted'\n"
  assert load_yaml_text(raw, default=None) == yaml.safe_load(raw)


def test_malformed_document_raises_yaml_error():
  with pytest.raises(yaml.YAMLError):
    load_yaml_text("a: [1, 2\nb: {c", default={})


def test_save_and_load_round_trip(tmp_path):
  path = tmp_path / "data.yaml"
  data = {"b": 1, "a": "中文 unicode", "n": [1, 2, {"k": "v"}], "empty": None}
  save_yaml(path, data)
  expected = yaml.safe_dump(data, allow_unicode=True, default_flow_style=False, sort_keys=False)
  assert path.read_text(encoding="utf-8") == expected
  assert load_yaml(path, default=None) == data
