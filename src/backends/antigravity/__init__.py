"""The antigravity backend package."""

from src.runtime.hooks import backend_type_registration, usage_source_registration

# The ledger's source value and the usage page's card title for antigravity's usage, which only
# CharlieBot's own logs hold.
USAGE_SOURCE = "Antigravity"


def register() -> None:
  """Register the antigravity backend type and the Antigravity usage source with the runtime."""
  backend_type_registration.register_backend_type(
      "antigravity",
      options="src.backends.antigravity.options:AntigravityBackend",
      factory="src.backends.antigravity.factory:build",
      traits=backend_type_registration.BackendTraits(
          resume="native_id",
          restart_reattach=False,
          preassigned_session_id=False,
          family_prefix=None,
          manual_compact=False,
          compact_hint=None,
      ),
  )
  usage_source_registration.register_source(
      usage_source_registration.UsageSource(
          name=USAGE_SOURCE, id_prefixes=("antigravity-", "agy-"), run_logs_only=True, module=None))
  usage_source_registration.attribute_backend_type("antigravity", USAGE_SOURCE)
