from src.infra import config_registry
from src.runtime.hooks import wiring

OWNER = "slack"


def register() -> None:
  from src.infra import metadata_slots

  wiring.register_command("slack", "src.features.slack.cli")
  wiring.register_service("slack", "src.features.slack.service")
  wiring.register_router("src.features.slack.api", prefix="/api/internal", tags=("internal",))
  config_registry.register_config_section(
      "slack",
      "src.features.slack.config:SlackConfig",
      legacy_keys={"slack_allowed_user_ids": "slack.allowed_user_ids"},
  )
  metadata_slots.register_metadata_fields(
      OWNER,
      "src.features.slack.metadata:SlackSessionFields",
      on=metadata_slots.ON_SESSION,
      after="successor_session_id")
