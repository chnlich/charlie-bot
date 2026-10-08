"""The opencode backend package."""


def register() -> None:
  """Register the opencode backend type with the runtime."""
  from src.runtime.hooks import backend_types

  backend_types.register_backend_type(
      "opencode",
      factory="src.backends.opencode.factory:build",
      traits=backend_types.BackendTraits(
          resume="native_id",
          restart_reattach=False,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
  )
