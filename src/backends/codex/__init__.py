"""The codex backend package."""

# The ledger's source value and the usage page's card title for the Codex CLI's own logs.
USAGE_SOURCE = "Codex"


def register() -> None:
  """Register the codex backend type and the Codex usage source with the runtime."""
  from src.runtime.hooks import backend_lifecycle, backend_types, usage_sources

  backend_types.register_backend_type(
      "codex",
      options="src.backends.codex.options:CodexBackend",
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
  usage_sources.register_source(
      usage_sources.UsageSource(
          name=USAGE_SOURCE, id_prefixes=("codex-",), run_logs_only=False, module="src.backends.codex.usage_logs"))
  usage_sources.attribute_backend_type("codex", USAGE_SOURCE)
