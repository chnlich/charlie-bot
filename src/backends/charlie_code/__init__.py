"""The charlie-code backend package."""

from src.infra import identity_env
from src.runtime.hooks import backend_type_registration, usage_source_registration

# The child's API-key injection name: the backend writes the configured key into the child env under
# it, and register() adds it to the identity variables an isolated trial must not inherit.
CHARLIE_CODE_API_KEY_ENV = "CHARLIE_CODE_API_KEY"

# The ledger's source value and the usage page's card title for Charlie Code's usage, which only
# CharlieBot's own logs hold. The name is the short spelling the interface uses.
USAGE_SOURCE = "CLC"


def register() -> None:
  """Register the charlie-code backend type, the CLC usage source and its API-key variable with the runtime.

  CLC's usage lives only in CharlieBot's own run logs, so its source has no log reader.
  """
  backend_type_registration.register_backend_type(
      "charlie-code",
      options="src.backends.charlie_code.options:CharlieCodeBackend",
      factory="src.backends.charlie_code.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="native_id",
          restart_reattach=True,
          preassigned_session_id=False,
          reads_context_window=True,
          family_prefix=None,
      ),
  )
  usage_source_registration.register_source(
      usage_source_registration.UsageSource(
          name=USAGE_SOURCE, id_prefixes=("charlie-code-",), run_logs_only=True, module=None))
  usage_source_registration.attribute_backend_type("charlie-code", USAGE_SOURCE)
  identity_env.register_identity_env_var(CHARLIE_CODE_API_KEY_ENV)
