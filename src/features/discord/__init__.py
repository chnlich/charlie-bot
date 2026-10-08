from src.infra import config_registry, metadata_slot_registration
from src.runtime.hooks import turn_contributions, wiring

OWNER = "discord"


def register() -> None:
  wiring.register_command("discord", "src.features.discord.cli")
  wiring.register_service("discord", "src.features.discord.service")
  wiring.register_router("src.features.discord.api", prefix="/api/internal", tags=("internal",))
  config_registry.register_config_section("discord", "src.features.discord.config:DiscordConfig")
  metadata_slot_registration.register_metadata_fields(
      OWNER,
      "src.features.discord.metadata:DiscordSessionFields",
      on=metadata_slot_registration.ON_SESSION,
      after="successor_session_id")
  turn_contributions.register_turn_contribution("discord", "src.features.discord.turn_contribution:CONTRIBUTION")
