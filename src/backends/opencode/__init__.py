"""The opencode backend package."""

# The ledger's source value and the usage page's card title for opencode's own database.
USAGE_SOURCE = "opencode"


def register() -> None:
  """Register the opencode backend type, its usage source and its reading limits."""
  from src.runtime.hooks import backend_lifecycle, backend_types, usage_sources

  backend_types.register_backend_type(
      "opencode",
      options="src.backends.opencode.options:OpencodeBackend",
      factory="src.backends.opencode.factory:build",
      traits=backend_types.BackendTraits(
          resume="native_id",
          restart_reattach=False,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix=None,
      ),
  )
  usage_sources.register_source(
      usage_sources.UsageSource(
          name=USAGE_SOURCE, id_prefixes=("opencode-",), run_logs_only=False,
          module="src.backends.opencode.usage_logs"))
  usage_sources.attribute_backend_type("opencode", USAGE_SOURCE)
  backend_lifecycle.register_reading_limits("snapshot", "src.backends.opencode.opencode_limits:snapshot_reading_limits")
