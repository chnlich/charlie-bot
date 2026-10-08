"""The charlie-code backend package."""


def register() -> None:
  """Register the charlie-code backend type with the runtime."""
  from src.runtime.hooks import backend_types

  backend_types.register_backend_type(
      "charlie-code",
      factory="src.backends.charlie_code.factory:build",
      traits=backend_types.BackendTraits(
          resume="native_id",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=True,
          family_prefix=None,
      ),
  )
