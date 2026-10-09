"""The Claude account pool's config: the ``accounts:`` section and the check across it and ``backends``.

``register()`` in this package's ``__init__.py`` registers ``AccountsConfig`` as the ``accounts``
section and ``check_claude_pools`` as a config check (``src/infra/config_registry.py``). The parsed
section is ``cfg.accounts``. This module imports pydantic only, so a config parse stays light.
"""

from typing import TYPE_CHECKING

import pydantic

if TYPE_CHECKING:
  from src.infra import config


class ClaudeAccount(pydantic.BaseModel):
  """One Claude subscription login in the account pool (src/backends/claude_code/claude_accounts.py).

  ``label`` names the account in server logs and the usage panel (it follows the
  label the panel derived from the directory name before the pool existed);
  ``config_dir`` is the login's CLAUDE_CONFIG_DIR. Order carries no meaning.
  """
  model_config = pydantic.ConfigDict(extra='forbid')

  label: str
  config_dir: str


class ClaudeCompactionConfig(pydantic.BaseModel):
  """Context floors, in tokens, for the Sonnet compaction the pool runs on Fable sessions.

  ``relay_tokens`` applies before an account relay (the cache is cold in the new
  login anyway); ``expired_cache_tokens`` applies when a user message arrives after
  the one-hour prompt cache has expired. Below the floor a cold read is cheaper
  than a compaction, so nothing runs.
  """
  model_config = pydantic.ConfigDict(extra='forbid')

  relay_tokens: int = pydantic.Field(default=100_000, gt=0)
  expired_cache_tokens: int = pydantic.Field(default=50_000, gt=0)


class AccountsConfig(pydantic.BaseModel):
  """``accounts:`` section: the Claude subscription pool and its compaction floors."""

  model_config = pydantic.ConfigDict(extra='forbid')

  # The Claude subscription logins (each label names one CLAUDE_CONFIG_DIR);
  # accounts.claude lists every login, and claude_pools groups them into the
  # named pools cc-claude entries draw from (src/backends/claude_code/claude_accounts.py).
  claude: list[ClaudeAccount] = []

  # Named account pools: each key is a pool name, its value the account labels
  # (from `claude` above) the pool holds. One label may sit in several pools.
  # Empty = no pools: every cc-claude entry draws from all of `claude`, exactly
  # as before pools existed. Cross-field checks against the backends options
  # (each cc-claude entry's `account_pool`) run in check_claude_pools, which
  # sees both sections.
  claude_pools: dict[str, list[str]] = {}

  # Token floors for the Sonnet compaction the pool runs on Fable sessions.
  claude_compaction: ClaudeCompactionConfig = ClaudeCompactionConfig()


def check_claude_pools(cfg: config.CharlieBotConfig) -> None:
  """Gate the Claude account pools across the ``accounts`` and ``backends`` sections.

  The pool table and the cc-claude options that name a pool live in different
  sections, so neither section's own model can check the pairing; a config
  that fails here is refused at load, and a failed hot reload keeps the
  previous config. Every error names the pool or the option id it concerns.
  """
  labels = {account.label for account in cfg.accounts.claude}
  for pool_name, pool_labels in cfg.accounts.claude_pools.items():
    unknown = [label for label in pool_labels if label not in labels]
    if unknown:
      raise ValueError(
          f"accounts.claude_pools['{pool_name}'] names accounts missing from accounts.claude: "
          f"{', '.join(unknown)}")
    if not pool_labels:
      raise ValueError(f"accounts.claude_pools['{pool_name}'] lists no account")
  for option in cfg.backends.options:
    if option.type != "cc-claude":
      continue
    if not cfg.accounts.claude_pools:
      if option.account_pool is not None:
        raise ValueError(
            f"backend '{option.id}' sets account_pool '{option.account_pool}' but "
            "accounts.claude_pools defines no pools")
      continue
    if option.account_pool is None:
      raise ValueError(
          f"backend '{option.id}' (cc-claude) names no account_pool; defined pools: "
          f"{', '.join(cfg.accounts.claude_pools)}")
    if option.account_pool not in cfg.accounts.claude_pools:
      raise ValueError(
          f"backend '{option.id}' names undefined account_pool '{option.account_pool}'; defined pools: "
          f"{', '.join(cfg.accounts.claude_pools)}")
