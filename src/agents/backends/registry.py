"""Backend registry — constructs the correct AgentBackend from a BackendOption."""

from typing import Any

from src.agents.backends.base import AgentBackend
from src.agents.backends.claude_code import ClaudeCodeBackend
from src.agents.backends.codex import CodexBackend
from src.agents.backends.kimi import KimiBackend
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption


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
    kwargs.pop("resume_session_id", None)
    return ClaudeCodeBackend(model=option.model, **kwargs)
  elif option.type == "cc-kimi":
    kwargs.pop("resume_session_id", None)
    if not cfg.moonshot_api_key:
      raise ValueError("moonshot_api_key not set in config")
    return KimiBackend(api_key=cfg.moonshot_api_key, model=option.model or cfg.kimi_model, **kwargs)
  elif option.type == "codex":
    return CodexBackend(
        model=option.model,
        resume_session_id=kwargs.pop("resume_session_id", None),
        **kwargs,
    )
  raise ValueError(f"Unknown backend type: {option.type}")
