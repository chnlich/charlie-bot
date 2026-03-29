"""Tests for CharlieBotConfig.server_base_url."""

from pathlib import Path

from src.core.config import CharlieBotConfig


def test_server_base_url_uses_localhost_for_internal_cli_calls() -> None:
  cfg = CharlieBotConfig(server_host="0.0.0.0", server_port=8123)
  assert cfg.server_base_url == "http://localhost:8123"


def test_server_base_url_uses_https_only_when_cert_and_key_are_set(tmp_path: Path) -> None:
  certfile = tmp_path / "cert.pem"
  keyfile = tmp_path / "key.pem"

  cfg_cert_only = CharlieBotConfig(server_port=9443, ssl_certfile=str(certfile))
  assert cfg_cert_only.server_base_url == "http://localhost:9443"

  cfg_cert_and_key = CharlieBotConfig(server_port=9443, ssl_certfile=str(certfile), ssl_keyfile=str(keyfile))
  assert cfg_cert_and_key.server_base_url == "https://localhost:9443"
