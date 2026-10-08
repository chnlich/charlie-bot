"""The cc-claude backend package."""


def register() -> None:
  """Register the cc-claude backend type with the runtime."""
  from src.runtime.hooks import backend_lifecycle, backend_types

  backend_types.register_backend_type(
      "cc-claude",
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
