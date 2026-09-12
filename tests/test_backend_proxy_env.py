"""The per-entry ``proxy_url`` env contract, single-homed across the CLI backends.

Both ``charlie-code`` and ``opencode`` inject the proxy through the shared
``apply_proxy_env`` (src/agents/backends/base.py); each parametrized case drives
that backend's own ``_prepare_env`` call site, so both wirings stay covered while
the assertions live in one place.
"""

from typing import Any

import pytest
from conftest import (
    CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
    OPENCODE_RESOLVE_BINARY_PATCH_TARGET,
    build_cli_backend,
)

from src.agents.backends.base import AgentBackend
from src.agents.backends.charlie_code import CharlieCodeBackend
from src.agents.backends.opencode import OpenCodeBackend

# Each row mirrors the backend's own test module's _build_backend construction
# (class, resolve_binary patch target, fake binary, constructor defaults).
BackendDescriptor = tuple[type[AgentBackend], str, str, dict[str, Any]]

_CLI_BACKENDS: list[pytest.param] = [
    pytest.param(
        (
            CharlieCodeBackend, CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/charlie-code", {
                "model": "charlie-code-test-model",
                "api_base": "http://test.invalid/v1",
            }),
        id="charlie-code",
    ),
    pytest.param((OpenCodeBackend, OPENCODE_RESOLVE_BINARY_PATCH_TARGET, "/usr/bin/opencode", {}), id="opencode"),
]


def _build_backend(monkeypatch: pytest.MonkeyPatch, descriptor: BackendDescriptor, **kwargs: Any) -> AgentBackend:
  backend_cls, patch_target, fake_binary, defaults = descriptor
  return build_cli_backend(monkeypatch, backend_cls, patch_target, fake_binary, defaults=defaults, **kwargs)


@pytest.mark.parametrize("descriptor", _CLI_BACKENDS)
def test_prepare_env_injects_proxy_and_merges_local_no_proxy_without_mutating_input(
    monkeypatch: pytest.MonkeyPatch, descriptor: BackendDescriptor) -> None:
  backend = _build_backend(monkeypatch, descriptor, proxy_url="http://proxy.test:8080")
  input_env = {
      "PATH": "/usr/bin",
      "NO_PROXY": "internal.test,localhost,127.0.0.1",
  }
  original_env = dict(input_env)

  prepared = backend._prepare_env(input_env)

  assert prepared["HTTP_PROXY"] == "http://proxy.test:8080"
  assert prepared["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert prepared["NO_PROXY"] == "internal.test,localhost,127.0.0.1,::1"
  assert input_env == original_env

  repeated = backend._prepare_env(prepared)
  assert repeated["NO_PROXY"] == prepared["NO_PROXY"]


@pytest.mark.parametrize("descriptor", _CLI_BACKENDS)
def test_prepare_env_without_proxy_preserves_proxy_related_environment(
    monkeypatch: pytest.MonkeyPatch, descriptor: BackendDescriptor) -> None:
  backend = _build_backend(monkeypatch, descriptor)
  input_env = {
      "PATH": "/usr/bin",
      "HTTP_PROXY": "http://existing-http.test:8080",
      "HTTPS_PROXY": "http://existing-https.test:8080",
      "NO_PROXY": "internal.test,localhost",
  }
  original_env = dict(input_env)

  prepared = backend._prepare_env(input_env)

  assert {
      key: prepared[key] for key in ("HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY")
  } == {
      "HTTP_PROXY": "http://existing-http.test:8080",
      "HTTPS_PROXY": "http://existing-https.test:8080",
      "NO_PROXY": "internal.test,localhost",
  }
  assert input_env == original_env


@pytest.mark.parametrize("descriptor", _CLI_BACKENDS)
def test_proxy_state_is_isolated_between_backend_instances(
    monkeypatch: pytest.MonkeyPatch, descriptor: BackendDescriptor) -> None:
  proxied = _build_backend(monkeypatch, descriptor, proxy_url="http://proxy.test:8080")
  unproxied = _build_backend(monkeypatch, descriptor)

  proxied_env = proxied._prepare_env({"PATH": "/usr/bin"})
  unproxied_env = unproxied._prepare_env({"PATH": "/usr/bin"})

  assert proxied_env["HTTP_PROXY"] == "http://proxy.test:8080"
  assert proxied_env["HTTPS_PROXY"] == "http://proxy.test:8080"
  assert proxied_env["NO_PROXY"] == "localhost,127.0.0.1,::1"
  assert "HTTP_PROXY" not in unproxied_env
  assert "HTTPS_PROXY" not in unproxied_env
  assert "NO_PROXY" not in unproxied_env
