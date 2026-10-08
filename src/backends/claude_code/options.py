"""The cc-claude option model: the ``backends.options[]`` entry of type "cc-claude"."""

from typing import Literal

from src.infra.backend_models import BackendOption


class CcClaudeBackend(BackendOption):
  type: Literal["cc-claude"] = "cc-claude"
  effort: str | None = None
  fast_mode: bool = False  # cc-claude only: enable Claude Code fast mode via --settings '{"fastMode":true}'
  cli_binary: str | None = None
  # cc-claude only: the named account pool (a key of accounts.claude_pools) this
  # entry draws its login from; selection and the rate-limit relay stay inside
  # that pool. Cross-field checks against accounts.claude_pools live in
  # claude_config.check_claude_pools (they cross two sections); None = no pools
  # are defined.
  account_pool: str | None = None
