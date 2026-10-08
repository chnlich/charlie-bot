"""The gemini backend package."""

from src.infra import config_registry
from src.runtime.hooks import backend_type_registration


def register() -> None:
  """Register the gemini backend type with the runtime."""
  backend_type_registration.register_backend_type(
      "gemini",
      options="src.backends.gemini.options:GeminiBackend",
      factory="src.backends.gemini.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="native_id",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
  )
  config_registry.register_legacy_keys(
      {
          config_registry.CREDENTIALS_PREFIX + "gemini_api_key": "gemini.api_key",
          config_registry.CREDENTIALS_PREFIX + "gemini_model": "gemini.model",
      })
