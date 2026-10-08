"""The cc-claude backend package.

The package owns two groups of metadata keys, declared here because ``register()`` reads their
field names and registering imports no backend module: ``ClaudeSessionFields`` are keys of a
session's ``metadata.json`` and ``ClaudeThreadFields`` are keys of a thread's. They are read and
written through ``src/backends/claude_code/claude_metadata.py``.
"""

from pydantic import BaseModel

OWNER = "claude_code"


class ClaudeSessionFields(BaseModel):
  # Label (claude_accounts[].label) of the pool account whose transcript store
  # holds this session's Claude Code conversation. None until the pool assigns
  # one, and always None for a pinned or non-cc-claude backend.
  claude_account: str | None = None


class ClaudeThreadFields(BaseModel):
  # The Claude Code session id the runtime chose for the task before its first process started.
  claude_session_id: str | None = None


# The ledger's source value and the usage page's card title for the Claude Code CLI's own logs.
USAGE_SOURCE = "Claude Code"


def register() -> None:
  """Register the cc-claude backend type, the Claude Code usage source, the accounts section,
  its metadata keys and its credential variables with the runtime."""
  from src.infra import config_registry, identity_env, metadata_slots
  from src.runtime.hooks import backend_lifecycle, backend_types, turn_contributions, usage_sources

  turn_contributions.register_turn_contribution(
      "claude_code", "src.backends.claude_code.turn_contribution:CONTRIBUTION")
  backend_types.register_backend_type(
      "cc-claude",
      options="src.backends.claude_code.options:CcClaudeBackend",
      factory="src.backends.claude_code.factory:build",
      traits=backend_types.BackendTraits(
          resume="cli_flag",
          restart_reattach=True,
          preassigned_session_id=True,
          reads_context_window=False,
          family_prefix=None,
      ),
      lifecycle="src.backends.claude_code.claude_lifecycle:ClaudeCodeLifecycle",
      translate_fallback=True,
  )
  backend_lifecycle.register_child_env("src.backends.claude_code.claude_code:claude_child_env")
  backend_lifecycle.register_reading_limits("claude", "src.backends.claude_code.claude_code:claude_reading_limits")
  usage_sources.register_source(
      usage_sources.UsageSource(
          name=USAGE_SOURCE,
          id_prefixes=("claude-",),
          run_logs_only=False,
          module="src.backends.claude_code.usage_logs"))
  usage_sources.attribute_backend_type("cc-claude", USAGE_SOURCE)
  config_registry.register_config_section(
      "accounts",
      "src.backends.claude_code.claude_config:AccountsConfig",
      legacy_keys={
          "claude_accounts": "accounts.claude",
          "claude_compaction": "accounts.claude_compaction",
      },
  )
  config_registry.register_config_check("src.backends.claude_code.claude_config:check_claude_pools")
  metadata_slots.register_metadata_fields(
      OWNER,
      "src.backends.claude_code:ClaudeSessionFields",
      on=metadata_slots.ON_SESSION,
      after="cc_session_started_at")
  metadata_slots.register_metadata_fields(
      OWNER, "src.backends.claude_code:ClaudeThreadFields", on=metadata_slots.ON_THREAD, after="exit_code")
  identity_env.register_identity_env_var("CLAUDE_CODE_OAUTH_TOKEN")
  identity_env.register_identity_env_var("ANTHROPIC_API_KEY")
