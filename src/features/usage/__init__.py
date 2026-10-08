from src.runtime.hooks import page_render, scheduled_handlers, wiring

# The ledger's source value for the records read from CharlieBot's own logs (src/features/usage/token_tally.py).
# The usage page attributes each such row's accounts to the CLI that ran the call, so its cards key on the
# registered source names, never on this value.
CHARLIE_BOT_SOURCE = "charlie-bot"


def register() -> None:
  wiring.register_router("src.features.usage.ext_usage", prefix="/api", tags=("ext-usage",))
  wiring.register_router("src.features.usage.api", tags=("pages",))
  wiring.register_service("usage_tally", "src.features.usage.api", phase="early")
  wiring.register_service("ext_usage", "src.features.usage.ext_usage")
  page_render.register_templates("src.features.usage")
  page_render.register_home_card(
      "Token usage by model", "/token-usage", "Tokens per model across every agent log on this host.")
  wiring.register_command("usage-ledger", "src.features.usage.cli")
  scheduled_handlers.register_handler(
      "usage_ledger", "src.features.usage.usage_ledger", attr="run_scheduled_usage_ledger")
  wiring.register_command("storage", "src.features.usage.storage_cli")
  scheduled_handlers.register_handler(
      "cool_storage", "src.features.usage.storage_cool", attr="run_scheduled_cool_storage")
