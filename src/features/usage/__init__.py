from src.runtime.hooks import scheduled_handlers, wiring


def register() -> None:
  wiring.register_router("src.features.usage.ext_usage", prefix="/api", tags=("ext-usage",))
  wiring.register_command("usage-ledger", "src.features.usage.cli")
  scheduled_handlers.register_handler(
      "usage_ledger", "src.features.usage.usage_ledger", attr="run_scheduled_usage_ledger")
