"""The cc-kimi backend package: a Claude CLI variant."""


def register() -> None:
  """Register the cc-kimi backend type with the runtime.

  The Claude CLI runs every cc-kimi call and logs it, so the type's usage counts under Claude Code.
  """
  from src.runtime.hooks import backend_types, usage_sources

  backend_types.register_backend_type(
      "cc-kimi",
      factory="src.backends.kimi.factory:build",
      traits=backend_types.BackendTraits(
          resume="cli_flag",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
      lifecycle="src.backends.claude_code.claude_lifecycle:ClaudeCliLifecycle",
  )
  usage_sources.attribute_backend_type("cc-kimi", "Claude Code")
