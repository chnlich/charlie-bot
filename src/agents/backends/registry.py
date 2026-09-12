"""Backend registry — constructs the correct AgentBackend from a BackendOption."""

from typing import Any

from src.agents.backends.antigravity_cli import AntigravityCliBackend
from src.agents.backends.base import AgentBackend
from src.agents.backends.charlie_code import CharlieCodeBackend
from src.agents.backends.claude_code import ClaudeCodeBackend
from src.agents.backends.codex import CodexBackend
from src.agents.backends.gemini_cli import GeminiCliBackend
from src.agents.backends.kimi import KimiBackend
from src.agents.backends.openai_compatible_claude import OpenAICompatibleClaudeBackend
from src.agents.backends.opencode import OpenCodeBackend
from src.agents.backends.tui import TuiBackend
from src.core.config import CharlieBotConfig, ClaudeAccount, get_credentials
from src.core.models import BackendOption, BackendType


def _require_model(option: BackendOption) -> str:
  if not option.model:
    raise ValueError(f"backend '{option.id}' has no default model")
  return option.model


def build_backend(
    option: BackendOption,
    cfg: CharlieBotConfig,
    *,
    claude_account: ClaudeAccount | None = None,
    **kwargs: Any,
) -> AgentBackend:
  """Instantiate the correct AgentBackend for *option*.

  Args:
    option: The BackendOption describing which backend to build.
    cfg: App configuration, used for the server base URL. Secrets come from
      ``get_credentials()`` (the credentials file), not from ``cfg``.
    claude_account: Claude login account providing the config directory for
      cc-claude backends; ``None`` leaves the backend without one.
    **kwargs: Extra keyword arguments forwarded to the backend constructor
      (e.g. extra_flags, buffer_limit, on_spawn).

  Returns:
    A concrete AgentBackend instance.

  Raises:
    ValueError: If the backend type is unknown or required config is missing.
  """
  if option.type == BackendType.CC_CLAUDE:
    return ClaudeCodeBackend(
        model=_require_model(option),
        effort=option.effort,
        cli_binary=option.cli_binary,
        fast_mode=option.fast_mode,
        claude_config_dir=claude_account.config_dir if claude_account else None,
        **kwargs)
  if option.type == BackendType.CC_KIMI:
    return KimiBackend(
        api_key=str(get_credentials().require(option.credential, "api_key")), model=_require_model(option), **kwargs)
  if option.type == BackendType.CC_OPENAI_COMPATIBLE:
    proxy_base_url = f"{cfg.server_base_url}/api/anthropic-proxy/openai-compatible/{option.id}"
    return OpenAICompatibleClaudeBackend(
        proxy_base_url=proxy_base_url,
        auth_token=str(get_credentials().require("charliebot", "access_key")),
        model=_require_model(option),
        **kwargs,
    )
  if option.type == BackendType.CODEX:
    return CodexBackend(
        model=_require_model(option),
        model_reasoning_effort=option.model_reasoning_effort,
        model_auto_compact_token_limit=option.model_auto_compact_token_limit,
        **kwargs)
  if option.type == BackendType.CHARLIE_CODE:
    return CharlieCodeBackend(
        model=_require_model(option),
        api_base=option.api_base,
        context_window=option.context_window,
        image_input=option.image_input,
        api_key=str(get_credentials().require(option.credential, "api_key")) if option.credential else None,
        **kwargs)
  if option.type == BackendType.GEMINI:
    return GeminiCliBackend(model=_require_model(option), **kwargs)
  if option.type == BackendType.OPENCODE:
    return OpenCodeBackend(model=_require_model(option), proxy_url=option.proxy_url, **kwargs)
  if option.type == BackendType.ANTIGRAVITY:
    return AntigravityCliBackend(print_timeout=option.print_timeout, **kwargs)
  if option.type == BackendType.TUI_CLI:
    return TuiBackend(**kwargs)
  raise ValueError(f"Unknown backend type: {option.type}")
