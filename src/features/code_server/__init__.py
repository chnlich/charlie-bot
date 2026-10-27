from src.infra import config_registry


def register() -> None:
  config_registry.register_config_section(
      "code_server",
      "src.features.code_server.config:CodeServerConfig",
      legacy_keys={
          "code_server_bin": "code_server.bin",
          "code_server_config": "code_server.config",
      },
  )
