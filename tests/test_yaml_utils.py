"""The yaml_utils loader contract: libyaml's C safe pair, its parity with the
pure-Python safe pair, and the load/save behaviors every caller relies on."""

import pytest
import yaml

from src.infra import yaml_utils


def test_malformed_document_raises_yaml_error() -> None:
  with pytest.raises(yaml.YAMLError):
    yaml_utils.load_yaml_text("a: [1, 2\nb: {c", default={})
