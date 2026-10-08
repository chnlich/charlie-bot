from src.runtime.hooks import scheduled_handlers, wiring


def register() -> None:
  wiring.register_command("storage", "src.features.storage.cli")
  scheduled_handlers.register_handler(
      "cool_storage", "src.features.storage.storage_cool", attr="run_scheduled_cool_storage")
