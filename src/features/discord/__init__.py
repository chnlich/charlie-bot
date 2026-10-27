from src.infra import config_registry, metadata_slot_registration
from src.runtime.hooks import turn_contributions

OWNER = "discord"


def register() -> None:
  config_registry.register_config_section("discord", "src.features.discord.config:DiscordConfig")
  metadata_slot_registration.register_metadata_fields(
      OWNER,
      "src.features.discord.metadata:DiscordSessionFields",
      on=metadata_slot_registration.ON_SESSION,
      after="successor_session_id")
  turn_contributions.register_turn_contribution("discord", "src.features.discord.turn_contribution:CONTRIBUTION")
