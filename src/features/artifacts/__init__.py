from src.infra import config_registry


def register() -> None:
  config_registry.register_config_section(
      "publish",
      "src.features.artifacts.config:PublishConfig",
      legacy_keys={
          "publish_dir": "publish.dir",
          "public_base_url": "publish.public_base_url",
      },
  )
