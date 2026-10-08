"""The opencode backend package."""

from src.runtime.hooks import backend_lifecycle_registration, backend_type_registration, usage_source_registration

# The ledger's source value and the usage page's card title for opencode's own database.
USAGE_SOURCE = "opencode"


def register() -> None:
  """Register the opencode backend type, its usage source and its reading limits."""
  backend_type_registration.register_backend_type(
      "opencode",
      options="src.backends.opencode.options:OpencodeBackend",
      factory="src.backends.opencode.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="native_id",
          restart_reattach=False,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
  )
  usage_source_registration.register_source(
      usage_source_registration.UsageSource(
          name=USAGE_SOURCE, id_prefixes=("opencode-",), run_logs_only=False,
          module="src.backends.opencode.usage_logs"))
  usage_source_registration.attribute_backend_type("opencode", USAGE_SOURCE)
  backend_lifecycle_registration.register_reading_limits(
      "snapshot", "src.backends.opencode.opencode_limits:snapshot_reading_limits")
