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
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption


def _require_model(option: BackendOption) -> str:
  if not option.model:
    raise ValueError(f"backend '{option.id}' has no default model")
  return option.model


def build_backend(option: BackendOption, cfg: CharlieBotConfig, **kwargs: Any) -> AgentBackend:
  """Instantiate the correct AgentBackend for *option*.

  Args:
    option: The BackendOption describing which backend to build.
    cfg: App configuration (used for API keys, etc.).
    **kwargs: Extra keyword arguments forwarded to the backend constructor
      (e.g. extra_flags, buffer_limit, on_spawn).

  Returns:
    A concrete AgentBackend instance.

  Raises:
    ValueError: If the backend type is unknown or required config is missing.
  """
  if option.type == "cc-claude":
    return ClaudeCodeBackend(
        model=_require_model(option),
        effort=option.effort,
        cli_binary=option.cli_binary,
        fast_mode=option.fast_mode,
        claude_config_dir=option.claude_config_dir,
        **kwargs)
  if option.type == "cc-kimi":
    model = _require_model(option)
    if not cfg.moonshot_api_key:
      raise ValueError("moonshot_api_key not set in config")
    return KimiBackend(api_key=cfg.moonshot_api_key, model=model, **kwargs)
  if option.type == "cc-openai-compatible":
    model = _require_model(option)
    if not cfg.charliebot_access_key:
      raise ValueError("charliebot_access_key not set in config")
    proxy_base_url = f"{cfg.server_base_url}/api/anthropic-proxy/openai-compatible/{option.id}"
    return OpenAICompatibleClaudeBackend(
        proxy_base_url=proxy_base_url,
        auth_token=cfg.charliebot_access_key,
        model=model,
        **kwargs,
    )
  if option.type == "codex":
    return CodexBackend(
        model=_require_model(option),
        codex_home=option.codex_home,
        model_reasoning_effort=option.model_reasoning_effort,
        model_auto_compact_token_limit=option.model_auto_compact_token_limit,
        **kwargs)
  if option.type == "charlie-code":
    return CharlieCodeBackend(
        model=_require_model(option),
        api_base=option.api_base,
        context_window=option.context_window,
        **kwargs)
  if option.type == "gemini":
    return GeminiCliBackend(model=_require_model(option), **kwargs)
  if option.type == "opencode":
    return OpenCodeBackend(model=_require_model(option), opencode_proxy_url=option.opencode_proxy_url, **kwargs)
  if option.type == "antigravity":
    return AntigravityCliBackend(**kwargs)
  if option.type == "tui-cli":
    return TuiBackend(**kwargs)
  raise ValueError(f"Unknown backend type: {option.type}")
