from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("router", "src.features.router.cli")
