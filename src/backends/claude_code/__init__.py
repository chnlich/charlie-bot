"""The cc-claude backend package.

The package owns two groups of metadata keys, ``ClaudeSessionFields`` (keys of a session's ``metadata.json``)
and ``ClaudeThreadFields`` (keys of a thread's). ``src/backends/claude_code/claude_metadata.py`` defines them
and reads and writes them; ``register()`` names them by string, so registering imports no pydantic.
"""

from src.infra import config_registry, identity_env, metadata_slot_registration
from src.runtime.hooks import (
    backend_lifecycle_registration,
    backend_type_registration,
    turn_contributions,
    usage_source_registration,
)

OWNER = "claude_code"

# The ledger's source value and the usage page's card title for the Claude Code CLI's own logs.
USAGE_SOURCE = "Claude Code"


def register() -> None:
  """Register the cc-claude backend type, the Claude Code usage source, the accounts section,
  its metadata keys and its credential variables with the runtime."""
  turn_contributions.register_turn_contribution(
      "claude_code", "src.backends.claude_code.turn_contribution:CONTRIBUTION")
  backend_type_registration.register_backend_type(
      "cc-claude",
      options="src.backends.claude_code.options:CcClaudeBackend",
      factory="src.backends.claude_code.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="cli_flag",
          restart_reattach=True,
          preassigned_session_id=True,
          reads_context_window=False,
          family_prefix=None,
      ),
      lifecycle="src.backends.claude_code.claude_lifecycle:ClaudeCodeLifecycle",
      translate_fallback=True,
  )
  backend_lifecycle_registration.register_child_env("src.backends.claude_code.claude_code:claude_child_env")
  backend_lifecycle_registration.register_reading_limits(
      "claude", "src.backends.claude_code.claude_code:claude_reading_limits")
  usage_source_registration.register_source(
      usage_source_registration.UsageSource(
          name=USAGE_SOURCE,
          id_prefixes=("claude-",),
          run_logs_only=False,
          module="src.backends.claude_code.usage_logs"))
  usage_source_registration.attribute_backend_type("cc-claude", USAGE_SOURCE)
  config_registry.register_config_section(
      "accounts",
      "src.backends.claude_code.claude_config:AccountsConfig",
      legacy_keys={
          "claude_accounts": "accounts.claude",
          "claude_compaction": "accounts.claude_compaction",
      },
  )
  config_registry.register_config_check("src.backends.claude_code.claude_config:check_claude_pools")
  metadata_slot_registration.register_metadata_fields(
      OWNER,
      "src.backends.claude_code.claude_metadata:ClaudeSessionFields",
      on=metadata_slot_registration.ON_SESSION,
      after="cc_session_started_at")
  metadata_slot_registration.register_metadata_fields(
      OWNER,
      "src.backends.claude_code.claude_metadata:ClaudeThreadFields",
      on=metadata_slot_registration.ON_THREAD,
      after="exit_code")
  identity_env.register_identity_env_var("CLAUDE_CODE_OAUTH_TOKEN")
  identity_env.register_identity_env_var("ANTHROPIC_API_KEY")
