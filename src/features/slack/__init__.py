from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("slack", "src.features.slack.cli")
  wiring.register_service("slack", "src.features.slack.service")
