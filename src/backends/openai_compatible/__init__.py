"""The cc-openai-compatible backend package."""

from src.runtime.hooks import backend_type_registration, usage_source_registration


def register() -> None:
  """Register the cc-openai-compatible backend type.

  The Claude CLI runs every cc-openai-compatible call and logs it, so the type's usage counts under Claude Code.
  """
  backend_type_registration.register_backend_type(
      "cc-openai-compatible",
      options="src.backends.openai_compatible.options:CcOpenAICompatibleBackend",
      factory="src.backends.openai_compatible.factory:build",
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
  usage_source_registration.attribute_backend_type("cc-openai-compatible", "Claude Code")
