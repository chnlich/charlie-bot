from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("improve", "src.features.improve.cli")
  wiring.register_command("improve-stop", "src.features.improve.stop_cli")
  wiring.register_router("src.features.improve.api", prefix="/api/internal", tags=("internal",))
