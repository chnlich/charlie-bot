"""Create the usage ledger of a home that has never captured: the schema, the aggregate triggers and the one-time backfill,
with no rows and no capture time. The token usage page then reads a steady-state ledger that says no capture has run.
"""

from rust.parity.fixture import Builder
from src.features.usage import usage_ledger


async def build(b: Builder) -> dict:
  ledger = usage_ledger.UsageLedger(usage_ledger.default_ledger_path())
  ledger.model_rows_with_native_starts()
  ledger.close()
  return {"ledger": "home/usage/ledger.sqlite3"}
