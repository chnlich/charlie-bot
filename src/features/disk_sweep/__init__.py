from src.runtime.hooks import scheduled_handlers


def register() -> None:
  scheduled_handlers.register_handler("disk_sweep", "src.features.disk_sweep.sweep", attr="run_scheduled")
