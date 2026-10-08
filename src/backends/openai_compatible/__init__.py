"""The cc-openai-compatible backend package and Anthropic proxy route."""


def register() -> None:
  """Register the proxy route and the cc-openai-compatible backend type."""
  from src.runtime.hooks import backend_types, wiring

  wiring.register_router(
      "src.backends.openai_compatible.anthropic_proxy", prefix="/api/anthropic-proxy", tags=("anthropic-proxy",))
  backend_types.register_backend_type(
      "cc-openai-compatible",
      factory="src.backends.openai_compatible.factory:build",
      traits=backend_types.BackendTraits(
          resume="cli_flag",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
      lifecycle="src.backends.claude_code.claude_lifecycle:ClaudeCliLifecycle",
  )
