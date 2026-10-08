from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.terminal.api")
