"""The yaml_utils loader contract: a malformed document surfaces as a yaml error."""

import pytest
import yaml

from src.infra import yaml_utils


def test_malformed_document_raises_yaml_error() -> None:
  with pytest.raises(yaml.YAMLError):
    yaml_utils.load_yaml_text("a: [1, 2\nb: {c", default={})
