from src.infra import config_registry, metadata_slot_registration
from src.runtime.hooks import turn_contributions, wiring

OWNER = "slack"


def register() -> None:
  wiring.register_command("slack", "src.features.slack.cli")
  wiring.register_service("slack", "src.features.slack.service")
  wiring.register_router("src.features.slack.api", prefix="/api/internal", tags=("internal",))
  config_registry.register_config_section(
      "slack",
      "src.features.slack.config:SlackConfig",
      legacy_keys={
          "slack_allowed_user_ids": "slack.allowed_user_ids",
          config_registry.CREDENTIALS_PREFIX + "slack_bot_token": "slack.bot_token",
          config_registry.CREDENTIALS_PREFIX + "slack_app_token": "slack.app_token",
          config_registry.CREDENTIALS_PREFIX + "slack_user_token": "slack.user_token",
      },
  )
  metadata_slot_registration.register_metadata_fields(
      OWNER,
      "src.features.slack.metadata:SlackSessionFields",
      on=metadata_slot_registration.ON_SESSION,
      after="successor_session_id")
  turn_contributions.register_turn_contribution("slack", "src.features.slack.turn_contribution:CONTRIBUTION")
