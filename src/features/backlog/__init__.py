from src.runtime.hooks import scheduled_handlers, wiring


def register() -> None:
  wiring.register_router("src.features.backlog.api", prefix="/api/backlog", tags=("backlog",))
  scheduled_handlers.register_loop_action("src.features.backlog.backlog_loop", attr="scheduled_loop_action")
