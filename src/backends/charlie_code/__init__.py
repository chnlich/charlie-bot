"""The charlie-code backend package."""

# The child's API-key injection name: the backend writes the configured key into the child env under
# it, and register() adds it to the identity variables an isolated trial must not inherit.
CHARLIE_CODE_API_KEY_ENV = "CHARLIE_CODE_API_KEY"


def register() -> None:
  """Register the charlie-code backend type, the CLC usage source and its API-key variable with the runtime.

  CLC's usage lives only in CharlieBot's own run logs, so its source has no log reader.
  """
  from src.infra import identity_env
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
  identity_env.register_identity_env_var(CHARLIE_CODE_API_KEY_ENV)
