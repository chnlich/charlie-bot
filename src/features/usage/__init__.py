from src.runtime.hooks import page_render, scheduled_handlers, wiring


def register() -> None:
  wiring.register_router("src.features.usage.ext_usage", prefix="/api", tags=("ext-usage",))
  wiring.register_router("src.features.usage.api", tags=("pages",))
  page_render.register_templates("src.features.usage")
  page_render.register_home_card(
      "Token usage by model", "/token-usage", "Tokens per model across every agent log on this host.")
  wiring.register_command("usage-ledger", "src.features.usage.cli")
  scheduled_handlers.register_handler(
      "usage_ledger", "src.features.usage.usage_ledger", attr="run_scheduled_usage_ledger")
