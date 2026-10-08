from src.runtime.hooks import wiring


def register() -> None:
  wiring.register_router("src.features.cron.api", prefix="/api/cron", tags=("cron",))
  wiring.register_service("scheduler", "src.features.cron.service")
