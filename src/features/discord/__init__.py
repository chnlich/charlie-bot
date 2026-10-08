from src.infra import config_registry
from src.runtime.hooks import turn_contributions, wiring

OWNER = "discord"


def register() -> None:
  from src.infra import metadata_slots

  wiring.register_command("discord", "src.features.discord.cli")
  wiring.register_service("discord", "src.features.discord.service")
  wiring.register_router("src.features.discord.api", prefix="/api/internal", tags=("internal",))
  config_registry.register_config_section("discord", "src.features.discord.config:DiscordConfig")
  metadata_slots.register_metadata_fields(
      OWNER,
      "src.features.discord.metadata:DiscordSessionFields",
      on=metadata_slots.ON_SESSION,
      after="successor_session_id")
  turn_contributions.register_turn_contribution("discord", "src.features.discord.turn_contribution:CONTRIBUTION")
