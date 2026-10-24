from src.runtime.hooks import scheduled_handlers, wiring


def register() -> None:
  wiring.register_command("disk-sweep", "src.features.disk_sweep.cli")
  scheduled_handlers.register_handler("disk_sweep", "src.features.disk_sweep.sweep", attr="run_scheduled")
