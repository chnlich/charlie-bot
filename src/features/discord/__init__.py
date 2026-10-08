from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("discord", "src.features.discord.cli")
  wiring.register_service("discord", "src.features.discord.service")
  wiring.register_router("src.features.discord.api", prefix="/api/internal", tags=("internal",))
