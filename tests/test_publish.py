"""Tests for the publish config section's validation: the command and lane keys."""

import pytest

from src.infra import config


@pytest.mark.parametrize(
    "lane_key, lane_value",
    [("dir", "/srv/charliebot_pub"), ("public_base_url", "https://pub.example.com/charliebot_pub")],
)
def test_command_with_a_directory_lane_key_is_a_config_error(lane_key: str, lane_value: str) -> None:
  with pytest.raises(ValueError) as exc_info:
    config.CharlieBotConfig(publish={"command": ["publish-program"], lane_key: lane_value})

  message = str(exc_info.value)
  assert "publish.command" in message
  assert f"publish.{lane_key}" in message


def test_empty_command_list_is_a_config_error() -> None:
  with pytest.raises(ValueError) as exc_info:
    config.CharlieBotConfig(publish={"command": []})

  assert "publish.command is an empty list" in str(exc_info.value)
