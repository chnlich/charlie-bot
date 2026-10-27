from src.runtime.hooks import scheduled_handlers

# The ledger's source value for the records read from CharlieBot's own logs (src/features/usage/token_tally.py).
# The usage page attributes each such row's accounts to the CLI that ran the call, so its cards key on the
# registered source names, never on this value.
CHARLIE_BOT_SOURCE = "charlie-bot"


def register() -> None:
  scheduled_handlers.register_handler(
      "usage_ledger", "src.features.usage.usage_ledger", attr="run_scheduled_usage_ledger")
  scheduled_handlers.register_handler(
      "cool_storage", "src.features.usage.storage_cool", attr="run_scheduled_cool_storage")
