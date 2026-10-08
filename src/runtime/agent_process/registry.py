"""Backend registry — constructs the correct AgentBackend from a BackendOption."""

from typing import Any

from src.backends.antigravity import antigravity_cli
from src.backends.charlie_code import charlie_code
from src.backends.claude_code import claude_code
from src.backends.codex import codex
from src.backends.gemini import gemini_cli
from src.backends.kimi import kimi
from src.backends.openai_compatible import openai_compatible_claude
from src.backends.opencode import opencode
from src.infra import config, constants, models
from src.runtime.agent_process import base


def build_backend(
    option: models.BackendOption,
    cfg: config.CharlieBotConfig,
    *,
    claude_account: config.ClaudeAccount | None = None,
    **kwargs: Any,
) -> base.AgentBackend:
  """Instantiate the correct AgentBackend for *option*.

  Args:
    option: The BackendOption describing which backend to build.
    cfg: App configuration, used for the server base URL. Secrets come from
      ``config.get_credentials()`` (the credentials file), not from ``cfg``.
    claude_account: Claude login account providing the config directory for
      cc-claude backends; ``None`` leaves the backend without one.
    **kwargs: Extra keyword arguments forwarded to the backend constructor
      (e.g. extra_flags, buffer_limit, on_spawn).

  Returns:
    A concrete AgentBackend instance.

  Raises:
    ValueError: If the backend type is unknown or required config is missing.
  """
  if option.type == constants.BackendType.CC_CLAUDE:
    return claude_code.ClaudeCodeBackend(
        model=models.option_default_model(option, subject="backend "),
        effort=option.effort,
        cli_binary=option.cli_binary,
        fast_mode=option.fast_mode,
        claude_config_dir=claude_account.config_dir if claude_account else None,
        **kwargs)
  if option.type == constants.BackendType.CC_KIMI:
    return kimi.KimiBackend(
        api_key=str(config.get_credentials().require(option.credential, "api_key")),
        model=models.option_default_model(option, subject="backend "),
        **kwargs)
  if option.type == constants.BackendType.CC_OPENAI_COMPATIBLE:
    proxy_base_url = f"{cfg.server_base_url}/api/anthropic-proxy/openai-compatible/{option.id}"
    return openai_compatible_claude.OpenAICompatibleClaudeBackend(
        proxy_base_url=proxy_base_url,
        auth_token=str(config.get_credentials().require("charliebot", "access_key")),
        model=models.option_default_model(option, subject="backend "),
        **kwargs,
    )
  if option.type == constants.BackendType.CODEX:
    return codex.CodexBackend(
        model=models.option_default_model(option, subject="backend "),
        model_reasoning_effort=option.model_reasoning_effort,
        model_auto_compact_token_limit=option.model_auto_compact_token_limit,
        **kwargs)
  if option.type == constants.BackendType.CHARLIE_CODE:
    return charlie_code.CharlieCodeBackend(
        model=models.option_default_model(option, subject="backend "),
        api_base=option.api_base,
        context_window=option.context_window,
        image_input=option.image_input,
        stream=option.stream,
        timeout_seconds=option.timeout_seconds,
        top_p=option.top_p,
        temperature=option.temperature,
        proxy_url=option.proxy_url,
        api_key=str(config.get_credentials().require(option.credential, "api_key")) if option.credential else None,
        **kwargs)
  if option.type == constants.BackendType.GEMINI:
    return gemini_cli.GeminiCliBackend(model=models.option_default_model(option, subject="backend "), **kwargs)
  if option.type == constants.BackendType.OPENCODE:
    return opencode.OpenCodeBackend(
        model=models.option_default_model(option, subject="backend "), proxy_url=option.proxy_url, **kwargs)
  if option.type == constants.BackendType.ANTIGRAVITY:
    return antigravity_cli.AntigravityCliBackend(print_timeout=option.print_timeout, **kwargs)
  raise ValueError(f"Unknown backend type: {option.type}")
