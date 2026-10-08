"""The cc-claude backend package."""


def register() -> None:
  """Register the cc-claude backend type, the Claude Code usage source and the accounts section."""
  from src.infra import config_registry
  from src.runtime.hooks import backend_lifecycle, backend_types, usage_sources

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
          name="Claude Code",
          id_prefixes=("claude-",),
          run_logs_only=False,
          module="src.backends.claude_code.usage_logs"))
  usage_sources.attribute_backend_type("cc-claude", "Claude Code")
  config_registry.register_config_section(
      "accounts",
      "src.backends.claude_code.claude_config:AccountsConfig",
      legacy_keys={
          "claude_accounts": "accounts.claude",
          "claude_compaction": "accounts.claude_compaction",
      },
  )
  config_registry.register_config_check("src.backends.claude_code.claude_config:check_claude_pools")
