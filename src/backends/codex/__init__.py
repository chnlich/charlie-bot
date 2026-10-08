"""The codex backend package."""


def register() -> None:
  """Register the codex backend type with the runtime."""
  from src.runtime.hooks import backend_lifecycle, backend_types

  backend_types.register_backend_type(
      "codex",
      factory="src.backends.codex.factory:build",
      traits=backend_types.BackendTraits(
          resume="native_id",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix="codex",
      ),
  )
  backend_lifecycle.register_usage_resolver("codex", "src.backends.codex.codex_usage:CodexUsageResolver")
