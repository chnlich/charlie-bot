"""Backend registry — constructs the correct AgentBackend from a BackendOption."""

from typing import Any

from src.agents.backends.base import AgentBackend
from src.agents.backends.antigravity_cli import AntigravityCliBackend
from src.agents.backends.claude_code import ClaudeCodeBackend
from src.agents.backends.codex import CodexBackend
from src.agents.backends.deepseek_sglang import DeepSeekSGLangBackend
from src.agents.backends.gemini_cli import GeminiCliBackend
from src.agents.backends.kimi import KimiBackend
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
    return ClaudeCodeBackend(model=_require_model(option), effort=option.effort, cli_binary=option.cli_binary, **kwargs)
  elif option.type == "cc-kimi":
    model = _require_model(option)
    if not cfg.moonshot_api_key:
      raise ValueError("moonshot_api_key not set in config")
    return KimiBackend(api_key=cfg.moonshot_api_key, model=model, **kwargs)
  elif option.type == "cc-deepseek-sglang":
    model = _require_model(option)
    if not cfg.deepseek_sglang_base_url:
      raise ValueError("deepseek_sglang_base_url not set in config")
    if not cfg.charliebot_access_key:
      raise ValueError("charliebot_access_key not set in config")
    proxy_base_url = cfg.deepseek_sglang_anthropic_proxy_base_url
    return DeepSeekSGLangBackend(
        proxy_base_url=proxy_base_url,
        auth_token=cfg.charliebot_access_key,
        model=model,
        **kwargs,
    )
  elif option.type == "codex":
    return CodexBackend(model=_require_model(option), **kwargs)
  elif option.type == "gemini":
    return GeminiCliBackend(model=_require_model(option), **kwargs)
  elif option.type == "opencode":
    return OpenCodeBackend(model=_require_model(option), **kwargs)
  elif option.type == "antigravity":
    return AntigravityCliBackend(**kwargs)
  elif option.type == "tui-cli":
    return TuiBackend(**kwargs)
  raise ValueError(f"Unknown backend type: {option.type}")
