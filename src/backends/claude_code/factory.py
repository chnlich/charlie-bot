"""The factory of the cc-claude backend type (registered in ``src/backends/claude_code/__init__.py``)."""

from typing import Any

from src.backends.claude_code import claude_code
from src.infra import config, models
from src.runtime.agent_process import base


def build(
    option: models.BackendOption | None,
    cfg: config.CharlieBotConfig,
    *,
    claude_account: config.ClaudeAccount | None = None,
    **kwargs: Any,
) -> base.AgentBackend:
  """Instantiate the ClaudeCodeBackend for *option*.

  ``claude_account`` is the pool login whose config directory the backend runs under; None leaves
  the backend on the default login. A None *option* builds the bare backend that the worker's
  translate-only fallback needs: it parses events and spawns nothing.
  """
  if option is None:
    return claude_code.ClaudeCodeBackend(**kwargs)
  return claude_code.ClaudeCodeBackend(
      model=models.option_default_model(option, subject="backend "),
      effort=option.effort,
      cli_binary=option.cli_binary,
      fast_mode=option.fast_mode,
      claude_config_dir=claude_account.config_dir if claude_account else None,
      **kwargs)
