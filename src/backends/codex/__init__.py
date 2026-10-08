"""The codex backend package."""

from src.runtime.hooks import backend_lifecycle_registration, backend_type_registration, usage_source_registration

# The ledger's source value and the usage page's card title for the Codex CLI's own logs.
USAGE_SOURCE = "Codex"


def register() -> None:
  """Register the codex backend type and the Codex usage source with the runtime."""
  backend_type_registration.register_backend_type(
      "codex",
      options="src.backends.codex.options:CodexBackend",
      factory="src.backends.codex.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="native_id",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=False,
          family_prefix="codex",
      ),
  )
  backend_lifecycle_registration.register_usage_resolver("codex", "src.backends.codex.codex_usage:CodexUsageResolver")
  usage_source_registration.register_source(
      usage_source_registration.UsageSource(
          name=USAGE_SOURCE, id_prefixes=("codex-",), run_logs_only=False, module="src.backends.codex.usage_logs"))
  usage_source_registration.attribute_backend_type("codex", USAGE_SOURCE)
