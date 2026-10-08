"""The gemini backend package."""


def register() -> None:
  """Register the gemini backend type with the runtime."""
  from src.runtime.hooks import backend_types

  backend_types.register_backend_type(
      "gemini",
      factory="src.backends.gemini.factory:build",
      traits=backend_types.BackendTraits(
          resume="native_id",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
  )
