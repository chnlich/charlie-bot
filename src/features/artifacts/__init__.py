from src.infra import config_registry
from src.runtime.hooks import sidebar_contributions, wiring


def register() -> None:
  wiring.register_command("artifact", "src.features.artifacts.cli")
  wiring.register_command("plan", "src.features.artifacts.plan_cli")
  wiring.register_command("publish", "src.features.artifacts.publish_cli")
  wiring.register_file_view("src.features.artifacts.artifact_view", attr="serve_artifact_path")
  wiring.register_router(
      "src.features.artifacts.api", prefix="/api/internal", tags=("internal",), attr="internal_router")
  wiring.register_router(
      "src.features.artifacts.api", prefix="/api/sessions", tags=("sessions",), attr="sessions_router")
  config_registry.register_config_section(
      "publish",
      "src.features.artifacts.config:PublishConfig",
      legacy_keys={
          "publish_dir": "publish.dir",
          "public_base_url": "publish.public_base_url",
      },
  )
  sidebar_contributions.register_sidebar_contribution("artifacts", "src.features.artifacts.sidebar:contribution")
