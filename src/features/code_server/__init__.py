from src.infra import config_registry
from src.runtime.hooks import page_render, wiring


def register() -> None:
  wiring.register_router("src.features.code_server.api", prefix="/api/code-server", tags=("code-server",))
  page_render.register_template_global(
      "code_server_enabled", "src.features.code_server.api", attr="code_server_enabled")
  config_registry.register_config_section(
      "code_server",
      "src.features.code_server.config:CodeServerConfig",
      legacy_keys={
          "code_server_bin": "code_server.bin",
          "code_server_config": "code_server.config",
      },
  )
