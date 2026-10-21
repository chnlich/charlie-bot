"""The cc-anthropic-compatible backend package."""

from src.runtime.hooks import backend_lifecycle_registration, backend_type_registration, usage_source_registration


def register() -> None:
  """Register Claude Code against Anthropic-compatible endpoints."""
  backend_type_registration.register_backend_type(
      "cc-anthropic-compatible",
      options="src.backends.anthropic_compatible.options:CcAnthropicCompatibleBackend",
      factory="src.backends.anthropic_compatible.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="cli_flag",
          restart_reattach=True,
          preassigned_session_id=False,
          family_prefix=None,
          manual_compact=False,
          compact_hint=None,
      ),
      lifecycle="src.backends.claude_code.claude_lifecycle:ClaudeCliLifecycle",
  )
  backend_lifecycle_registration.register_backend_reading_limits(
      "cc-anthropic-compatible", "claude", "src.backends.anthropic_compatible.usage:claude_reading_limits")
  usage_source_registration.attribute_backend_type("cc-anthropic-compatible", "Claude Code")
