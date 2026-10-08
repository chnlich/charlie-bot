from src.infra import config_registry
from src.runtime.hooks import scheduled_handlers, wiring


def register() -> None:
  wiring.register_router("src.features.backlog.api", prefix="/api/backlog", tags=("backlog",))
  scheduled_handlers.register_loop_action("src.features.backlog.backlog_loop", attr="scheduled_loop_action")
  config_registry.register_config_section(
      "backlog",
      "src.features.backlog.config:BacklogConfig",
      legacy_keys={
          "backlog_repos": "backlog.repos",
          "backlog_repo": "removed (list the repo under backlog.repos)",
          "backlog_label": "removed",
      },
  )
