import json
from typing import Any

import httpx
import pytest
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents.backends.openai_compatible_claude import OpenAICompatibleClaudeBackend
from src.agents.backends.registry import build_backend
from src.api.anthropic_proxy import router as proxy_router
from src.core.config import CharlieBotConfig, get_config
from src.core.models import BackendOption

_PROXY_PREFIX = "/api/anthropic-proxy"
_BACKEND_ID = "cc-glm52"
_DIRECT_PROXY_BASE_URL = f"http://localhost:8000{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}"
_MESSAGES_PATH = f"{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}/v1/messages"
_PROXY_MODEL = "nvidia/GLM-5.2-NVFP4"
_UPSTREAM_BASE = "http://upstream.example/v1"
_AUTH_TOKEN = "charliebot-key"


def _option(**overrides) -> BackendOption:
  base: dict[str, Any] = {
      "id": _BACKEND_ID,
      "label": "CC GLM-5.2",
      "type": "cc-openai-compatible",
      "model": _PROXY_MODEL,
      "api_base": _UPSTREAM_BASE,
  }
  base.update(overrides)
  return BackendOption(**base)


def _cfg(option: BackendOption | None = None, **overrides) -> CharlieBotConfig:
  return CharlieBotConfig(
      server_port=8123,
      charliebot_access_key=overrides.pop("charliebot_access_key", _AUTH_TOKEN),
      backend_options=[option or _option()],
  )


def test_prepare_env_sets_proxy_endpoint_and_model() -> None:
  backend = OpenAICompatibleClaudeBackend(
      proxy_base_url=_DIRECT_PROXY_BASE_URL,
      auth_token=_AUTH_TOKEN,
      model=_PROXY_MODEL,
  )

  prepared = backend._prepare_env({"PATH": "/usr/bin"})

  assert prepared["PATH"] == "/usr/bin"
  assert prepared["CLAUDE_CODE_DISABLE_BACKGROUND_TASKS"] == "1"
  assert prepared["ANTHROPIC_BASE_URL"] == _DIRECT_PROXY_BASE_URL
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == _AUTH_TOKEN
  assert prepared["ANTHROPIC_MODEL"] == _PROXY_MODEL
  assert prepared["ANTHROPIC_DEFAULT_OPUS_MODEL"] == _PROXY_MODEL
  assert prepared["ANTHROPIC_DEFAULT_SONNET_MODEL"] == _PROXY_MODEL
  assert prepared["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == _PROXY_MODEL
  assert prepared["CLAUDE_CODE_SUBAGENT_MODEL"] == _PROXY_MODEL


def test_build_command_does_not_pass_model_flag() -> None:
  backend = OpenAICompatibleClaudeBackend(
      proxy_base_url=_DIRECT_PROXY_BASE_URL,
      auth_token=_AUTH_TOKEN,
      model=_PROXY_MODEL,
  )

  cmd = backend._build_command("hello")

  assert "--model" not in cmd
  assert "hello" not in cmd
  assert backend._stdin_prompt("hello") == "hello"


def test_requires_model_proxy_and_auth_token() -> None:
  with pytest.raises(ValueError, match="requires a model"):
    OpenAICompatibleClaudeBackend(proxy_base_url="http://localhost:8000/proxy", auth_token="key", model="")
  with pytest.raises(ValueError, match="proxy_base_url"):
    OpenAICompatibleClaudeBackend(proxy_base_url="", auth_token="key", model=_PROXY_MODEL)
  with pytest.raises(ValueError, match="charliebot_access_key"):
    OpenAICompatibleClaudeBackend(proxy_base_url="http://localhost:8000/proxy", auth_token="", model=_PROXY_MODEL)


def test_registry_builds_openai_compatible_backend() -> None:
  option = _option()
  cfg = _cfg(option)

  backend = build_backend(option, cfg)

  assert isinstance(backend, OpenAICompatibleClaudeBackend)
  prepared = backend._prepare_env({})
  assert prepared["ANTHROPIC_BASE_URL"] == f"http://localhost:8123{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}"
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == _AUTH_TOKEN
  assert prepared["ANTHROPIC_MODEL"] == _PROXY_MODEL


def test_registry_requires_model_and_access_key() -> None:
  option_no_model = _option(model=None)
  with pytest.raises(ValueError, match="no default model"):
    build_backend(option_no_model, _cfg(option_no_model, charliebot_access_key=_AUTH_TOKEN))

  option_with_model = _option()
  with pytest.raises(ValueError, match="charliebot_access_key"):
    build_backend(option_with_model, _cfg(option_with_model, charliebot_access_key=""))


# ---------------------------------------------------------------------------
# Proxy route tests
# ---------------------------------------------------------------------------


def _build_client(cfg: CharlieBotConfig) -> TestClient:
  app = FastAPI()
  app.include_router(proxy_router, prefix=_PROXY_PREFIX)
  app.dependency_overrides[get_config] = lambda: cfg
  return TestClient(app)


def _mock_upstream(monkeypatch: pytest.MonkeyPatch, handler) -> None:
  client = httpx.AsyncClient(transport=httpx.MockTransport(handler))

  def _factory() -> httpx.AsyncClient:
    return client

  monkeypatch.setattr("src.api.anthropic_proxy.get_http_client", _factory)


def _ok_response() -> dict:
  return {
      "id": "chatcmpl_1",
      "model": _PROXY_MODEL,
      "choices": [{
          "finish_reason": "stop",
          "message": {
              "content": "hi there"
          }
      }],
      "usage": {
          "prompt_tokens": 5,
          "completion_tokens": 2
      },
  }


def _anthropic_payload() -> dict:
  return {
      "model": "claude-facing",
      "messages": [{
          "role": "user",
          "content": "hi"
      }],
      "max_tokens": 16,
  }


def test_route_forwards_upstream_model_and_bearer_auth_and_translates_response(
    monkeypatch: pytest.MonkeyPatch,) -> None:
  monkeypatch.setenv("GLM_INTERNAL_KEY", "secret-token")
  cfg = _cfg(_option(api_key_env="GLM_INTERNAL_KEY"))
  captured: dict[str, Any] = {}

  def handler(request: httpx.Request) -> httpx.Response:
    captured["url"] = str(request.url)
    captured["authorization"] = request.headers.get("authorization")
    captured["json"] = json.loads(request.content)
    return httpx.Response(200, json=_ok_response())

  _mock_upstream(monkeypatch, handler)

  with _build_client(cfg) as client:
    response = client.post(
        _MESSAGES_PATH,
        json=_anthropic_payload(),
    )

  assert response.status_code == 200
  assert captured["url"] == f"{_UPSTREAM_BASE}/chat/completions"
  assert captured["json"]["model"] == _PROXY_MODEL
  assert captured["authorization"] == "Bearer secret-token"
  body = response.json()
  assert body["model"] == _PROXY_MODEL
  assert body["content"] == [{"type": "text", "text": "hi there"}]
  assert body["stop_reason"] == "end_turn"


def test_route_omits_authorization_when_api_key_env_absent(monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = _cfg(_option())
  captured: dict[str, Any] = {}

  def handler(request: httpx.Request) -> httpx.Response:
    captured["authorization"] = request.headers.get("authorization")
    return httpx.Response(200, json=_ok_response())

  _mock_upstream(monkeypatch, handler)

  with _build_client(cfg) as client:
    response = client.post(
        _MESSAGES_PATH,
        json=_anthropic_payload(),
    )

  assert response.status_code == 200
  assert captured["authorization"] is None


def test_route_fails_loud_when_api_key_env_missing(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.delenv("DEFINITELY_MISSING_KEY", raising=False)
  cfg = _cfg(_option(api_key_env="DEFINITELY_MISSING_KEY"))

  with _build_client(cfg) as client:
    response = client.post(
        _MESSAGES_PATH,
        json=_anthropic_payload(),
    )

  assert response.status_code == 400
  assert "DEFINITELY_MISSING_KEY" in response.json()["detail"]


def test_route_returns_404_for_unknown_backend_id() -> None:
  cfg = CharlieBotConfig(server_port=8123, charliebot_access_key="key", backend_options=[])

  with _build_client(cfg) as client:
    response = client.post(
        f"{_PROXY_PREFIX}/openai-compatible/nope/v1/messages",
        json=_anthropic_payload(),
    )

  assert response.status_code == 404
  assert "unknown backend id" in response.json()["detail"]


def test_route_rejects_wrong_backend_type() -> None:
  cfg = CharlieBotConfig(
      server_port=8123,
      charliebot_access_key="key",
      backend_options=[
          BackendOption(id="opus", label="Opus", type="cc-claude", model="claude-opus-4-8"),
      ],
  )

  with _build_client(cfg) as client:
    response = client.post(
        f"{_PROXY_PREFIX}/openai-compatible/opus/v1/messages",
        json=_anthropic_payload(),
    )

  assert response.status_code == 400
  assert "not type 'cc-openai-compatible'" in response.json()["detail"]


def test_route_requires_api_base() -> None:
  cfg = _cfg(_option(api_base=None))

  with _build_client(cfg) as client:
    response = client.post(
        _MESSAGES_PATH,
        json=_anthropic_payload(),
    )

  assert response.status_code == 400
  assert "missing api_base" in response.json()["detail"]


def test_route_requires_model() -> None:
  cfg = _cfg(_option(model=None))

  with _build_client(cfg) as client:
    response = client.post(
        _MESSAGES_PATH,
        json=_anthropic_payload(),
    )

  assert response.status_code == 400
  assert "missing model" in response.json()["detail"]
