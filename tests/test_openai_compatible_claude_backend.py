from typing import Any

import conftest
import fastapi
import httpx
import pytest
from fastapi import testclient

from src.backends.openai_compatible import anthropic_proxy, openai_compatible_claude
from src.infra import config
from src.runtime.hooks import backend_types

_PROXY_PREFIX = "/api/anthropic-proxy"
_BACKEND_ID = "cc-glm52"
_DIRECT_PROXY_BASE_URL = f"http://localhost:8000{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}"
_MESSAGES_PATH = f"{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}/v1/messages"
_PROXY_MODEL = "nvidia/GLM-5.2-NVFP4"
_UPSTREAM_BASE = "http://upstream.example/v1"
_AUTH_TOKEN = "charliebot-key"


def _option(**overrides: Any) -> Any:
  base: dict[str, Any] = {
      "id": _BACKEND_ID,
      "label": "CC GLM-5.2",
      "type": "cc-openai-compatible",
      "model": _PROXY_MODEL,
      "api_base": _UPSTREAM_BASE,
  }
  base.update(overrides)
  return conftest.backend_option(**base)


def _cfg(option: Any | None = None) -> config.CharlieBotConfig:
  return config.CharlieBotConfig(server={"port": 8123}, backends={"options": [option or _option()]})


def test_prepare_env_sets_proxy_endpoint_and_token() -> None:
  backend = openai_compatible_claude.OpenAICompatibleClaudeBackend(
      proxy_base_url=_DIRECT_PROXY_BASE_URL,
      auth_token=_AUTH_TOKEN,
      model=_PROXY_MODEL,
  )

  prepared = backend._prepare_env({"PATH": "/usr/bin"})

  assert prepared["ANTHROPIC_BASE_URL"] == _DIRECT_PROXY_BASE_URL
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == _AUTH_TOKEN
  assert prepared["ANTHROPIC_MODEL"] == _PROXY_MODEL


def test_requires_model_proxy_and_auth_token() -> None:
  with pytest.raises(ValueError, match="requires a model"):
    openai_compatible_claude.OpenAICompatibleClaudeBackend(
        proxy_base_url="http://localhost:8000/proxy", auth_token="key", model="")
  with pytest.raises(ValueError, match="proxy_base_url"):
    openai_compatible_claude.OpenAICompatibleClaudeBackend(proxy_base_url="", auth_token="key", model=_PROXY_MODEL)
  with pytest.raises(ValueError, match="auth_token"):
    openai_compatible_claude.OpenAICompatibleClaudeBackend(
        proxy_base_url="http://localhost:8000/proxy", auth_token="", model=_PROXY_MODEL)


def test_backend_types_builds_openai_compatible_backend() -> None:
  option = _option()
  cfg = _cfg(option)
  conftest.stub_credentials({"charliebot": {"access_key": _AUTH_TOKEN}})

  backend = backend_types.build_backend(option, cfg)

  assert isinstance(backend, openai_compatible_claude.OpenAICompatibleClaudeBackend)
  prepared = backend._prepare_env({})
  assert prepared["ANTHROPIC_BASE_URL"] == f"http://localhost:8123{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}"
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == _AUTH_TOKEN
  assert prepared["ANTHROPIC_MODEL"] == _PROXY_MODEL


# ---------------------------------------------------------------------------
# Proxy route tests
# ---------------------------------------------------------------------------


def _build_client(cfg: config.CharlieBotConfig) -> testclient.TestClient:
  app = fastapi.FastAPI()
  app.include_router(anthropic_proxy.router, prefix=_PROXY_PREFIX)
  conftest.apply_config_overrides(app, cfg)
  return testclient.TestClient(app)


def _anthropic_payload() -> dict:
  return {
      "model": "claude-facing",
      "messages": [{
          "role": "user",
          "content": "hi"
      }],
      "max_tokens": 16,
  }


def _post_messages(cfg: config.CharlieBotConfig, path: str) -> httpx.Response:
  """Post the shared Anthropic payload to *path* on the proxy TestClient."""
  with _build_client(cfg) as client:
    return client.post(path, json=_anthropic_payload())


def test_route_fails_loud_when_credential_missing() -> None:
  conftest.stub_credentials({})
  cfg = _cfg(_option(credential="missing_upstream"))

  response = _post_messages(cfg, _MESSAGES_PATH)

  assert response.status_code == 400
  assert "missing_upstream" in response.json()["detail"]
