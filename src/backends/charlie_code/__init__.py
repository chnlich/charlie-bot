"""The charlie-code backend package."""


def register() -> None:
  """Register the charlie-code backend type and the CLC usage source with the runtime.

  CLC's usage lives only in CharlieBot's own run logs, so its source has no log reader.
  """
  from src.runtime.hooks import backend_types, usage_sources

  backend_types.register_backend_type(
      "charlie-code",
      options="src.backends.charlie_code.options:CharlieCodeBackend",
      factory="src.backends.charlie_code.factory:build",
      traits=backend_types.BackendTraits(
          resume="native_id",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=True,
          family_prefix=None,
      ),
  )
  usage_sources.register_source(
      usage_sources.UsageSource(name="CLC", id_prefixes=("charlie-code-",), run_logs_only=True, module=None))
  usage_sources.attribute_backend_type("charlie-code", "CLC")
