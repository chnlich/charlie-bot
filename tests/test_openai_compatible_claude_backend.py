import json
from typing import Any

import httpx
import pytest
from conftest import backend_option, stub_credentials
from fastapi import FastAPI
from fastapi.testclient import TestClient

from src.agents.backends.openai_compatible_claude import OpenAICompatibleClaudeBackend
from src.agents.backends.registry import build_backend
from src.api.anthropic_proxy import router as proxy_router
from src.core.config import CharlieBotConfig, get_config

_PROXY_PREFIX = "/api/anthropic-proxy"
_BACKEND_ID = "cc-glm52"
_DIRECT_PROXY_BASE_URL = f"http://localhost:8000{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}"
_MESSAGES_PATH = f"{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}/v1/messages"
_PROXY_MODEL = "nvidia/GLM-5.2-NVFP4"
_UPSTREAM_BASE = "http://upstream.example/v1"
_AUTH_TOKEN = "charliebot-key"


def _option(**overrides) -> Any:
  base: dict[str, Any] = {
      "id": _BACKEND_ID,
      "label": "CC GLM-5.2",
      "type": "cc-openai-compatible",
      "model": _PROXY_MODEL,
      "api_base": _UPSTREAM_BASE,
  }
  base.update(overrides)
  return backend_option(**base)


def _cfg(option: Any | None = None) -> CharlieBotConfig:
  return CharlieBotConfig(server={"port": 8123}, backends={"options": [option or _option()]})


def test_prepare_env_sets_proxy_endpoint_and_token() -> None:
  backend = OpenAICompatibleClaudeBackend(
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
    OpenAICompatibleClaudeBackend(proxy_base_url="http://localhost:8000/proxy", auth_token="key", model="")
  with pytest.raises(ValueError, match="proxy_base_url"):
    OpenAICompatibleClaudeBackend(proxy_base_url="", auth_token="key", model=_PROXY_MODEL)
  with pytest.raises(ValueError, match="auth_token"):
    OpenAICompatibleClaudeBackend(proxy_base_url="http://localhost:8000/proxy", auth_token="", model=_PROXY_MODEL)


def test_registry_builds_openai_compatible_backend() -> None:
  option = _option()
  cfg = _cfg(option)
  stub_credentials({"charliebot": {"access_key": _AUTH_TOKEN}})

  backend = build_backend(option, cfg)

  assert isinstance(backend, OpenAICompatibleClaudeBackend)
  prepared = backend._prepare_env({})
  assert prepared["ANTHROPIC_BASE_URL"] == f"http://localhost:8123{_PROXY_PREFIX}/openai-compatible/{_BACKEND_ID}"
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == _AUTH_TOKEN
  assert prepared["ANTHROPIC_MODEL"] == _PROXY_MODEL


def test_registry_requires_access_key_from_credentials() -> None:
  stub_credentials({})

  with pytest.raises(ValueError, match=r"credentials\.charliebot\.access_key"):
    build_backend(_option(), _cfg())


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


def test_route_forwards_upstream_model_and_bearer_auth_and_translates_response(monkeypatch: pytest.MonkeyPatch) -> None:
  stub_credentials({"glm-upstream": {"api_key": "secret-token"}})
  cfg = _cfg(_option(credential="glm-upstream"))
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


def test_route_omits_authorization_when_credential_unset(monkeypatch: pytest.MonkeyPatch) -> None:
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


def test_route_fails_loud_when_credential_missing(monkeypatch: pytest.MonkeyPatch) -> None:
  stub_credentials({})
  cfg = _cfg(_option(credential="missing_upstream"))

  with _build_client(cfg) as client:
    response = client.post(
        _MESSAGES_PATH,
        json=_anthropic_payload(),
    )

  assert response.status_code == 400
  assert "missing_upstream" in response.json()["detail"]


def test_route_returns_404_for_unknown_backend_id() -> None:
  cfg = CharlieBotConfig(server={"port": 8123}, backends={"options": []})

  with _build_client(cfg) as client:
    response = client.post(
        f"{_PROXY_PREFIX}/openai-compatible/nope/v1/messages",
        json=_anthropic_payload(),
    )

  assert response.status_code == 404
  assert "unknown backend id" in response.json()["detail"]


def test_route_rejects_wrong_backend_type() -> None:
  cfg = CharlieBotConfig(
      server={"port": 8123},
      backends={"options": [backend_option(id="opus", label="Opus", type="cc-claude", model="claude-opus-4-8")]},
  )

  with _build_client(cfg) as client:
    response = client.post(
        f"{_PROXY_PREFIX}/openai-compatible/opus/v1/messages",
        json=_anthropic_payload(),
    )

  assert response.status_code == 400
  assert "not type 'cc-openai-compatible'" in response.json()["detail"]
