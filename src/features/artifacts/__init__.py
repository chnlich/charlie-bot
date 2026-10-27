from src.infra import config_registry
from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_command("artifact", "src.features.artifacts.cli")
  wiring.register_command("plan", "src.features.artifacts.plan_cli")
  wiring.register_command("publish", "src.features.artifacts.publish_cli")
  config_registry.register_config_section(
      "publish",
      "src.features.artifacts.config:PublishConfig",
      legacy_keys={
          "publish_dir": "publish.dir",
          "public_base_url": "publish.public_base_url",
      },
  )
