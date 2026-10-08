"""The antigravity backend package."""

from src.runtime.hooks import backend_type_registration


def register() -> None:
  """Register the antigravity backend type with the runtime."""
  backend_type_registration.register_backend_type(
      "antigravity",
      options="src.backends.antigravity.options:AntigravityBackend",
      factory="src.backends.antigravity.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="native_id",
          restart_reattach=False,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
  )
