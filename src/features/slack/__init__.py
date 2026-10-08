from src.infra import config_registry
from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("slack", "src.features.slack.cli")
  wiring.register_service("slack", "src.features.slack.service")
  wiring.register_router("src.features.slack.api", prefix="/api/internal", tags=("internal",))
  config_registry.register_config_section(
      "slack",
      "src.features.slack.config:SlackConfig",
      legacy_keys={"slack_allowed_user_ids": "slack.allowed_user_ids"},
  )
