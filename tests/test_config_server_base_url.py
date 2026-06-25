"""Tests for CharlieBotConfig.server_base_url."""

from src.core.config import CharlieBotConfig


def test_server_base_url_uses_localhost_for_internal_cli_calls() -> None:
  cfg = CharlieBotConfig(server_port=8123)
  assert cfg.server_base_url == "http://localhost:8123"
