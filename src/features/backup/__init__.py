from src.runtime.hooks import scheduled_handlers


def register() -> None:
  scheduled_handlers.register_handler("backup", "src.features.backup.backup", attr="run_scheduled_backup")
