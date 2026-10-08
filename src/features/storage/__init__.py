from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("storage", "src.features.storage.cli")
