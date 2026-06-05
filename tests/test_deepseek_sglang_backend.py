import pytest

from src.agents.backends.deepseek_sglang import DeepSeekSGLangBackend
from src.agents.backends.registry import build_backend
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption


def test_prepare_env_sets_charliebot_proxy_endpoint_and_model() -> None:
  backend = DeepSeekSGLangBackend(
      proxy_base_url="http://localhost:8000/api/anthropic-proxy/deepseek-sglang",
      auth_token="charliebot-key",
      model="meshy-sglang/deepseek-ai/DeepSeek-V4-Pro",
  )

  prepared = backend._prepare_env({"PATH": "/usr/bin"})

  assert prepared["PATH"] == "/usr/bin"
  assert prepared["ANTHROPIC_BASE_URL"] == "http://localhost:8000/api/anthropic-proxy/deepseek-sglang"
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == "charliebot-key"
  assert prepared["ANTHROPIC_MODEL"] == "meshy-sglang/deepseek-ai/DeepSeek-V4-Pro"
  assert prepared["ANTHROPIC_DEFAULT_OPUS_MODEL"] == "meshy-sglang/deepseek-ai/DeepSeek-V4-Pro"
  assert prepared["ANTHROPIC_DEFAULT_SONNET_MODEL"] == "meshy-sglang/deepseek-ai/DeepSeek-V4-Pro"
  assert prepared["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == "meshy-sglang/deepseek-ai/DeepSeek-V4-Pro"
  assert prepared["CLAUDE_CODE_SUBAGENT_MODEL"] == "meshy-sglang/deepseek-ai/DeepSeek-V4-Pro"


def test_build_command_does_not_pass_model_flag() -> None:
  backend = DeepSeekSGLangBackend(
      proxy_base_url="http://localhost:8000/api/anthropic-proxy/deepseek-sglang",
      auth_token="charliebot-key",
      model="meshy-sglang/deepseek-ai/DeepSeek-V4-Pro",
  )

  cmd = backend._build_command("hello")

  assert "--model" not in cmd
  assert cmd[-2:] == ["--", "hello"]


def test_requires_model_proxy_and_auth_token() -> None:
  with pytest.raises(ValueError, match="requires a model"):
    DeepSeekSGLangBackend(proxy_base_url="http://localhost:8000/proxy", auth_token="key", model="")
  with pytest.raises(ValueError, match="proxy_base_url"):
    DeepSeekSGLangBackend(proxy_base_url="", auth_token="key", model="deepseek")
  with pytest.raises(ValueError, match="charliebot_access_key"):
    DeepSeekSGLangBackend(proxy_base_url="http://localhost:8000/proxy", auth_token="", model="deepseek")


def test_registry_builds_deepseek_sglang_backend() -> None:
  option = BackendOption(
      id="cc-deepseek-v4-pro",
      label="CC DeepSeek V4 Pro",
      type="cc-deepseek-sglang",
      model="meshy-sglang/deepseek-ai/DeepSeek-V4-Pro",
  )
  cfg = CharlieBotConfig(
      server_port=8123,
      server_external_base_url="https://charliebot.example",
      charliebot_access_key="charliebot-key",
      deepseek_sglang_base_url="http://sglang.example/v1",
  )

  backend = build_backend(option, cfg)

  assert isinstance(backend, DeepSeekSGLangBackend)
  prepared = backend._prepare_env({})
  assert prepared["ANTHROPIC_BASE_URL"] == "https://charliebot.example/api/anthropic-proxy/deepseek-sglang"
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == "charliebot-key"


def test_registry_requires_sglang_url_charliebot_auth_and_external_server_url() -> None:
  option = BackendOption(id="cc-deepseek", label="DeepSeek", type="cc-deepseek-sglang", model="deepseek")

  with pytest.raises(ValueError, match="deepseek_sglang_base_url"):
    build_backend(
        option,
        CharlieBotConfig(
            server_external_base_url="https://charliebot.example",
            charliebot_access_key="charliebot-key",
        ),
    )
  with pytest.raises(ValueError, match="charliebot_access_key"):
    build_backend(
        option,
        CharlieBotConfig(
            server_external_base_url="https://charliebot.example",
            deepseek_sglang_base_url="http://sglang.example/v1",
        ),
    )
  with pytest.raises(ValueError, match="server_external_base_url"):
    build_backend(
        option,
        CharlieBotConfig(
            charliebot_access_key="charliebot-key",
            deepseek_sglang_base_url="http://sglang.example/v1",
        ),
    )
  with pytest.raises(ValueError, match="externally reachable"):
    build_backend(
        option,
        CharlieBotConfig(
            server_external_base_url="http://localhost:8123",
            charliebot_access_key="charliebot-key",
            deepseek_sglang_base_url="http://sglang.example/v1",
        ),
    )
