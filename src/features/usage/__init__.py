from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.usage.ext_usage", prefix="/api", tags=("ext-usage",))
  wiring.register_command("usage-ledger", "src.features.usage.cli")
