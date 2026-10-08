from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("memory", "src.features.memory.cli")
