import pytest

from src.agents.backends.kimi import KimiBackend


def test_prepare_env_routes_api_key_to_moonshot_endpoint() -> None:
  backend = KimiBackend(api_key="moonshot-test-key", model="kimi-k2.5")

  prepared = backend._prepare_env({"PATH": "/usr/bin"})

  assert prepared["ANTHROPIC_BASE_URL"] == "https://api.moonshot.cn/anthropic"
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == "moonshot-test-key"
  assert prepared["ANTHROPIC_MODEL"] == "kimi-k2.5"


def test_requires_model() -> None:
  with pytest.raises(ValueError, match="requires a model"):
    KimiBackend(api_key="moonshot-test-key", model="")
