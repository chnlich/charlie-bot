"""The yaml_utils loader contract: libyaml's C safe pair, its parity with the
pure-Python safe pair, and the load/save behaviors every caller relies on."""


import pytest
import yaml

from src.core.yaml_utils import load_yaml_text


def test_malformed_document_raises_yaml_error() -> None:
  with pytest.raises(yaml.YAMLError):
    load_yaml_text("a: [1, 2\nb: {c", default={})
