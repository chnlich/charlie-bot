from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("slack", "src.features.slack.cli")
  wiring.register_service("slack", "src.features.slack.service")
  wiring.register_router("src.features.slack.api", prefix="/api/internal", tags=("internal",))
