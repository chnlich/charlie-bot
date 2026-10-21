from types import SimpleNamespace

import pydantic
import pytest

from src.backends.anthropic_compatible import factory, options
from src.backends.claude_code import claude_launch
from src.app import registrations
from src.infra import config, config_registry, event_types as ET
from src.runtime import session_usage
from src.runtime.hooks import (
    backend_lifecycle,
    backend_lifecycle_registration,
    backend_type_registration,
    usage_source_registration,
)

MODEL = "moonshotai/Kimi-K3"
API_BASE = "https://fpt-jp-slurm-kimi-k3.onca-snapper.ts.net"
CONTEXT_WINDOW = 409600


def _option(**overrides: object) -> options.CcAnthropicCompatibleBackend:
  fields: dict[str, object] = {
      "id": "anthropic-compatible-k3",
      "label": "CC · Kimi",
      "type": "cc-anthropic-compatible",
      "model": MODEL,
      "api_base": API_BASE,
      "context_window": CONTEXT_WINDOW,
  }
  fields.update(overrides)
  return options.CcAnthropicCompatibleBackend.model_validate(fields)


def test_factory_routes_endpoint_model_window_and_no_auth(monkeypatch: pytest.MonkeyPatch) -> None:

  def credentials_must_not_be_read() -> None:
    raise AssertionError("a credential-free option must not read credentials")

  monkeypatch.setattr(config, "get_credentials", credentials_must_not_be_read)
  monkeypatch.setenv(claude_launch.AUTO_COMPACT_WINDOW_ENV, "500000")
  monkeypatch.setenv(claude_launch.AUTOCOMPACT_PCT_OVERRIDE_ENV, "80")
  monkeypatch.setenv(claude_launch.MAX_CONTEXT_TOKENS_ENV, "100000")
  monkeypatch.setenv(claude_launch.THINKING_DISPLAY_UPDATES_ENV, "1")
  monkeypatch.setenv("ANTHROPIC_API_KEY", "inherited-value")
  monkeypatch.setenv("CLAUDE_CODE_OAUTH_TOKEN", "inherited-value")

  backend = factory.build(_option(), config.CharlieBotConfig())
  prepared = backend._prepare_env(
      {
          "ANTHROPIC_BASE_URL": "https://api.anthropic.com",
          "ANTHROPIC_AUTH_TOKEN": "stale-value",
      })

  assert prepared["ANTHROPIC_BASE_URL"] == API_BASE
  assert prepared["ANTHROPIC_AUTH_TOKEN"] == "no-auth"
  assert prepared["ANTHROPIC_MODEL"] == MODEL
  assert prepared["ANTHROPIC_DEFAULT_OPUS_MODEL"] == MODEL
  assert prepared["ANTHROPIC_DEFAULT_SONNET_MODEL"] == MODEL
  assert prepared["ANTHROPIC_DEFAULT_HAIKU_MODEL"] == MODEL
  assert prepared["CLAUDE_CODE_SUBAGENT_MODEL"] == MODEL
  assert prepared[claude_launch.AUTO_COMPACT_WINDOW_ENV] == str(CONTEXT_WINDOW)
  assert claude_launch.AUTOCOMPACT_PCT_OVERRIDE_ENV not in prepared
  assert claude_launch.MAX_CONTEXT_TOKENS_ENV not in prepared
  assert "ANTHROPIC_API_KEY" not in prepared
  assert "CLAUDE_CODE_OAUTH_TOKEN" not in prepared
  assert prepared[claude_launch.THINKING_DISPLAY_UPDATES_ENV] == "0"


def test_factory_routes_named_api_key(monkeypatch: pytest.MonkeyPatch) -> None:

  def credentials() -> SimpleNamespace:
    return SimpleNamespace(require=lambda section, key: "configured-value")

  monkeypatch.setattr(config, "get_credentials", credentials)
  backend = factory.build(_option(credential="kimi"), config.CharlieBotConfig())

  assert backend._prepare_env({})["ANTHROPIC_AUTH_TOKEN"] == "configured-value"


def test_option_requires_a_positive_window_and_endpoint_root() -> None:
  with pytest.raises(pydantic.ValidationError):
    _option(context_window=0)
  with pytest.raises(pydantic.ValidationError, match="endpoint root without /v1"):
    _option(api_base=f"{API_BASE}/v1")


def test_backend_type_registration_and_usage_attribution() -> None:
  registration = backend_type_registration.registration_of("cc-anthropic-compatible")

  assert "src.backends.anthropic_compatible" in registrations.PACKAGES
  assert config_registry.option_model("cc-anthropic-compatible") is options.CcAnthropicCompatibleBackend
  assert registration.factory == "src.backends.anthropic_compatible.factory:build"
  assert registration.lifecycle == "src.backends.claude_code.claude_lifecycle:ClaudeCliLifecycle"
  assert registration.traits == backend_type_registration.BackendTraits(
      resume="cli_flag",
      restart_reattach=True,
      preassigned_session_id=False,
      family_prefix=None,
      manual_compact=False,
      compact_hint=None,
  )
  limits = backend_lifecycle_registration.reading_limits("claude", "manager_turn", _option())
  assert limits == backend_lifecycle.ContextLimits(declared_window=CONTEXT_WINDOW, compact_reserve=33_000)
  source = usage_source_registration.source_for("cc-anthropic-compatible")
  assert source.name == "Claude Code"


def test_usage_gauge_uses_the_option_context_window(monkeypatch: pytest.MonkeyPatch) -> None:
  monkeypatch.setenv(claude_launch.AUTOCOMPACT_PCT_OVERRIDE_ENV, "80")
  monkeypatch.setenv(claude_launch.MAX_CONTEXT_TOKENS_ENV, "100000")
  option = _option()
  events = [
      {
          "type": ET.ASSISTANT,
          "message":
              {
                  "model": MODEL,
                  "usage":
                      {
                          ET.USAGE_INPUT_TOKENS: 1200,
                          ET.USAGE_CACHE_CREATION_INPUT_TOKENS: 0,
                          ET.USAGE_CACHE_READ_INPUT_TOKENS: 0,
                      },
              },
      },
      {
          "type": ET.RESULT,
          "modelUsage": {
              MODEL: {
                  "contextWindow": 1_000_000
              }
          }
      },
  ]

  reading = session_usage.resolve_events_usage(events, "manager_turn", backend_option=option)

  assert reading[ET.CONTEXT_FULL] == CONTEXT_WINDOW
  assert reading[ET.CONTEXT_COMPACT_AT] == CONTEXT_WINDOW - 20_000 - 13_000
