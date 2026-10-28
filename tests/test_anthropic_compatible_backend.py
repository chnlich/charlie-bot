import json
import os
import types
from typing import Any

import pydantic
import pytest

from src.app import registrations
from src.backends.anthropic_compatible import factory, options
from src.backends.claude_code import claude_code, claude_launch
from src.infra import config, config_registry
from src.infra import event_types
from src.runtime import session_usage
from src.runtime.hooks import (
    backend_lifecycle,
    backend_lifecycle_registration,
    backend_type_registration,
    usage_source_registration,
)

MODEL = "moonshotai/Kimi-K3"
API_BASE = "https://llm-gateway.example.invalid"
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
  assert prepared[claude_launch.MAX_CONTEXT_TOKENS_ENV] == str(CONTEXT_WINDOW)
  assert "ANTHROPIC_API_KEY" not in prepared
  assert "CLAUDE_CODE_OAUTH_TOKEN" not in prepared
  assert prepared[claude_launch.THINKING_DISPLAY_UPDATES_ENV] == "0"


def test_endpoint_env_overrides_every_inherited_gateway_variable(monkeypatch: pytest.MonkeyPatch) -> None:
  """A cc-anthropic-compatible run keeps its own endpoint, token and model even when it
  starts inside another cc-anthropic-compatible worker whose variables it inherits."""
  for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES:
    monkeypatch.setenv(name, "inherited-gateway-value")
  backend = factory.build(_option(), config.CharlieBotConfig())

  inherited_env = {name: os.environ[name] for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES}
  prepared = backend._prepare_env(inherited_env)

  assert prepared[claude_code.GATEWAY_BASE_URL_ENV] == API_BASE
  assert prepared[claude_code.GATEWAY_AUTH_TOKEN_ENV] == "no-auth"
  for name in claude_code.CLAUDE_MODEL_ENV_NAMES:
    assert prepared[name] == MODEL


def test_factory_routes_custom_headers() -> None:
  backend = factory.build(_option(headers={"x-litellm-tags": "deepseek"}), config.CharlieBotConfig())

  prepared = backend._prepare_env({})

  assert prepared[claude_code.GATEWAY_CUSTOM_HEADERS_ENV] == "x-litellm-tags: deepseek"


def test_factory_routes_multiple_custom_headers() -> None:
  backend = factory.build(
      _option(headers={
          "x-litellm-tags": "deepseek",
          "x-client-name": "charliebot"
      }), config.CharlieBotConfig())

  prepared = backend._prepare_env({})

  assert prepared[claude_code.GATEWAY_CUSTOM_HEADERS_ENV] == "x-litellm-tags: deepseek\nx-client-name: charliebot"


def test_factory_omits_custom_headers_when_option_has_none_and_parent_sets_them() -> None:
  backend = factory.build(_option(), config.CharlieBotConfig())

  prepared = backend._prepare_env({claude_code.GATEWAY_CUSTOM_HEADERS_ENV: "inherited-value"})

  assert claude_code.GATEWAY_CUSTOM_HEADERS_ENV not in prepared


def test_factory_routes_extra_body() -> None:
  extra_body = {"cache": {"no-store": True, "no-cache": True}}
  backend = factory.build(_option(extra_body=extra_body), config.CharlieBotConfig())

  prepared = backend._prepare_env({})

  assert json.loads(prepared[claude_code.GATEWAY_EXTRA_BODY_ENV]) == extra_body


def test_factory_omits_extra_body_when_option_has_none_and_parent_sets_it() -> None:
  backend = factory.build(_option(), config.CharlieBotConfig())

  prepared = backend._prepare_env({claude_code.GATEWAY_EXTRA_BODY_ENV: "inherited-value"})

  assert claude_code.GATEWAY_EXTRA_BODY_ENV not in prepared


def test_claude_model_env_sets_exactly_five_model_variables() -> None:
  assert claude_code.claude_model_env(MODEL) == {
      "ANTHROPIC_MODEL": MODEL,
      "ANTHROPIC_DEFAULT_OPUS_MODEL": MODEL,
      "ANTHROPIC_DEFAULT_SONNET_MODEL": MODEL,
      "ANTHROPIC_DEFAULT_HAIKU_MODEL": MODEL,
      "CLAUDE_CODE_SUBAGENT_MODEL": MODEL,
  }


def test_factory_routes_named_api_key(monkeypatch: pytest.MonkeyPatch) -> None:

  def credentials() -> types.SimpleNamespace:
    return types.SimpleNamespace(require=lambda section, key: "configured-value")

  monkeypatch.setattr(config, "get_credentials", credentials)
  backend = factory.build(_option(credential="kimi"), config.CharlieBotConfig())

  assert backend._prepare_env({})["ANTHROPIC_AUTH_TOKEN"] == "configured-value"


def test_factory_routes_effort_to_cli_command() -> None:
  backend = factory.build(_option(effort="high"), config.CharlieBotConfig())

  effort_index = backend._cmd.index("--effort")
  assert backend._cmd[effort_index + 1] == "high"


def test_factory_omits_effort_from_cli_command_by_default() -> None:
  backend = factory.build(_option(), config.CharlieBotConfig())

  assert "--effort" not in backend._cmd


class _FakeProc:

  returncode = 0

  async def communicate(self, input: bytes | None = None) -> tuple[bytes, bytes]:  # noqa: A002
    return b"ok", b""


def _capture_one_shot_launch(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
  """Patch the subprocess seam so a one-shot records its argv and env without starting `claude`."""
  launch: dict[str, Any] = {}

  async def _fake_exec(*args: object, **kwargs: object) -> _FakeProc:
    launch["args"] = args
    launch["env"] = kwargs["env"]
    return _FakeProc()

  monkeypatch.setattr(claude_code.asyncio, "create_subprocess_exec", _fake_exec)
  return launch


@pytest.mark.asyncio
async def test_one_shot_text_launches_on_the_entry_endpoint_without_a_model_flag(
    monkeypatch: pytest.MonkeyPatch,) -> None:
  extra_body = {"cache": {"no-store": True, "no-cache": True}}
  backend = factory.build(
      _option(effort="high", headers={"x-litellm-tags": "deepseek"}, extra_body=extra_body), config.CharlieBotConfig())
  launch = _capture_one_shot_launch(monkeypatch)

  assert await backend.one_shot_text("reply with ok", "system", timeout=None) == "ok"

  args = launch["args"]
  assert "--model" not in args
  assert args[args.index("--effort") + 1] == "high"
  env = launch["env"]
  assert env[claude_code.GATEWAY_BASE_URL_ENV] == API_BASE
  assert env[claude_code.GATEWAY_AUTH_TOKEN_ENV] == "no-auth"
  for name in claude_code.CLAUDE_MODEL_ENV_NAMES:
    assert env[name] == MODEL
  assert env[claude_code.GATEWAY_CUSTOM_HEADERS_ENV] == "x-litellm-tags: deepseek"
  assert json.loads(env[claude_code.GATEWAY_EXTRA_BODY_ENV]) == extra_body
  assert env[claude_launch.AUTO_COMPACT_WINDOW_ENV] == str(CONTEXT_WINDOW)
  assert env[claude_launch.MAX_CONTEXT_TOKENS_ENV] == str(CONTEXT_WINDOW)


@pytest.mark.asyncio
async def test_one_shot_text_omits_effort_when_the_entry_has_none(monkeypatch: pytest.MonkeyPatch) -> None:
  backend = factory.build(_option(), config.CharlieBotConfig())
  launch = _capture_one_shot_launch(monkeypatch)

  await backend.one_shot_text("reply with ok", "system", timeout=None)

  assert "--effort" not in launch["args"]


@pytest.mark.asyncio
async def test_cc_claude_one_shot_text_keeps_its_model_and_drops_gateway_variables(
    monkeypatch: pytest.MonkeyPatch,) -> None:
  for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES:
    monkeypatch.setenv(name, "inherited-gateway-value")
  launch = _capture_one_shot_launch(monkeypatch)
  backend = claude_code.ClaudeCodeBackend(model="claude-haiku-5-5")

  await backend.one_shot_text("reply with ok", "system", timeout=None)

  args = launch["args"]
  assert args[args.index("--model") + 1] == "claude-haiku-5-5"
  for name in claude_code.GATEWAY_ENDPOINT_ENV_NAMES:
    assert name not in launch["env"]


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
          "type": event_types.ASSISTANT,
          "message":
              {
                  "model": MODEL,
                  "usage":
                      {
                          event_types.USAGE_INPUT_TOKENS: 1200,
                          event_types.USAGE_CACHE_CREATION_INPUT_TOKENS: 0,
                          event_types.USAGE_CACHE_READ_INPUT_TOKENS: 0,
                      },
              },
      },
      {
          "type": event_types.RESULT,
          "modelUsage": {
              MODEL: {
                  "contextWindow": 1_000_000
              }
          }
      },
  ]

  reading = session_usage.resolve_events_usage(events, "manager_turn", backend_option=option)

  assert reading[event_types.CONTEXT_FULL] == CONTEXT_WINDOW
  assert reading[event_types.CONTEXT_COMPACT_AT] == CONTEXT_WINDOW - 20_000 - 13_000
