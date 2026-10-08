"""The cc-openai-compatible backend package and Anthropic proxy route."""

from src.runtime.hooks import backend_type_registration, usage_source_registration, wiring


def register() -> None:
  """Register the proxy route and the cc-openai-compatible backend type.

  The Claude CLI runs every cc-openai-compatible call and logs it, so the type's usage counts under Claude Code.
  """
  wiring.register_router(
      "src.backends.openai_compatible.anthropic_proxy", prefix="/api/anthropic-proxy", tags=("anthropic-proxy",))
  backend_type_registration.register_backend_type(
      "cc-openai-compatible",
      options="src.backends.openai_compatible.options:CcOpenAICompatibleBackend",
      factory="src.backends.openai_compatible.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="cli_flag",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
      lifecycle="src.backends.claude_code.claude_lifecycle:ClaudeCliLifecycle",
  )
  usage_source_registration.attribute_backend_type("cc-openai-compatible", "Claude Code")
