"""Write the usage ledger the token-usage parity scenarios read, through the Python ledger.

The rows cover every shape the page groups: native rows of three CLIs, two opencode spellings and a provider suffix that
merge into one model, charlie-bot accounts that attribute by a registered id, by an id prefix and by an id that left the
config, a counted fallback row, and a finished capture on this host. Every row's time is fixed, and the capture time is
pinned, so the page's only clock reads are the render time and the read time, which the normalize rules mask.
"""

import datetime
import socket
import types
from unittest import mock

from rust.parity.fixture import Builder
from src.features.usage import usage_ledger
from src.runtime.hooks import usage_sources

NATIVE = usage_sources.RecordKind.NATIVE
FALLBACK = usage_sources.RecordKind.FALLBACK

# The instant every ledger stamp takes: record_file's captured_at and mark_capture_finished's last_capture_at.
CAPTURED_AT = "2026-10-18T09:30:00+00:00"


class _PinnedDatetime(datetime.datetime):
  """The ``datetime`` class the ledger reads its stamps from: ``now`` returns the pinned capture instant."""

  @classmethod
  def now(cls, tz: datetime.tzinfo | None = None) -> datetime.datetime:
    return cls.fromisoformat(CAPTURED_AT)


# Stands in for the ``datetime`` module inside usage_ledger, which uses only ``datetime.datetime`` and ``datetime.UTC``.
_PINNED_MODULE = types.SimpleNamespace(datetime=_PinnedDatetime, UTC=datetime.UTC)


def _record(
    kind: usage_sources.RecordKind,
    record_id: str,
    source: str,
    model: str,
    account: str,
    ts: str,
    tokens: tuple[int, int, int, int, int],
    sessions: tuple[str, ...] = (),
) -> usage_sources.UsageRecord:
  """One record; *tokens* is ``(in_fresh, cache_write, cache_read, in_unsplit, output)``, the page's column order."""
  in_fresh, cache_write, cache_read, in_unsplit, output = tokens
  return usage_sources.UsageRecord(
      record_id=record_id,
      kind=kind,
      source=source,
      model=model,
      account=account,
      ts=ts,
      in_fresh=in_fresh,
      cache_write=cache_write,
      cache_read=cache_read,
      in_unsplit=in_unsplit,
      output=output,
      sessions=sessions,
  )


async def build(b: Builder) -> dict:
  # The ledger sits at the default path under the home the builder configured.
  path = usage_ledger.default_ledger_path()
  host = socket.gethostname()
  files = {
      "synthetic/claude/session-a.jsonl":
          [
              _record(
                  NATIVE, "claude:a/1", "Claude Code", "claude-fable-5-1", "synthetic-claude",
                  "2026-10-17T08:00:00+00:00", (1200, 800, 30000, 0, 400), ("claude-session-a",)),
              _record(
                  NATIVE, "claude:a/2", "Claude Code", "claude-fable-5-1", "synthetic-claude",
                  "2026-10-17T08:05:00+00:00", (900, 0, 31000, 0, 250), ("claude-session-a",)),
              _record(
                  NATIVE, "claude:a/3", "Claude Code", "claude-sonnet-5", "synthetic-claude",
                  "2026-10-16T19:00:00+00:00", (640, 0, 12000, 0, 180), ("claude-session-a",)),
          ],
      "synthetic/codex/session-b.jsonl":
          [
              _record(
                  NATIVE, "codex:b/1", "Codex", "gpt-synthetic-5.2", "synthetic-codex", "2026-10-17T09:00:00+00:00",
                  (2000, 0, 15000, 0, 900), ("codex-session-b",)),
          ],
      "synthetic/opencode/session-c.jsonl":
          [
              _record(
                  NATIVE, "opencode:c/1", "opencode", "zai-org/GLM-5.3-Flash", "synthetic-opencode",
                  "2026-10-17T10:00:00+00:00", (5000, 0, 0, 700, 1500), ("opencode-session-c",)),
              _record(
                  NATIVE, "opencode:c/2", "opencode", "GLM-5.3-Flash", "synthetic-opencode",
                  "2026-10-17T10:30:00+00:00", (300, 0, 4000, 0, 200), ("opencode-session-c",)),
              _record(
                  NATIVE, "opencode:d/1", "opencode", "Kimi-K3 (amd-kimi-k3)", "synthetic-opencode",
                  "2026-10-17T11:00:00+00:00", (800, 0, 9000, 0, 600), ("opencode-session-d",)),
          ],
      "synthetic/charlie-bot/thread-d.jsonl":
          [
              _record(
                  NATIVE, "thread:d/1", "charlie-bot", "claude-sonnet-5", "claude-sonnet", "2026-10-18T07:00:00+00:00",
                  (1500, 500, 20000, 0, 700)),
              _record(
                  FALLBACK, "thread:d/2", "charlie-bot", "gpt-synthetic-5.2", "codex-retired",
                  "2026-10-15T12:00:00+00:00", (1000, 0, 6000, 0, 300), ("codex-retired-session-e",)),
          ],
      "synthetic/charlie-bot/master-e.jsonl":
          [
              _record(
                  NATIVE, "master:e/1", "charlie-bot", "gemini-synthetic-flash", "charlie-code-old",
                  "2026-10-16T10:00:00+00:00", (400, 0, 0, 0, 150)),
          ],
      "synthetic/charlie-bot/antigravity-f.jsonl":
          [
              _record(
                  NATIVE, "thread:f/1", "charlie-bot", "gemini-synthetic-flash", "antigravity-desk",
                  "2026-10-16T11:00:00+00:00", (250, 0, 0, 0, 90)),
          ],
  }
  # The ledger stamps each row and the capture on the wall clock; the patch makes both stamps the pinned instant, so the
  # two trees write the same bytes.
  with mock.patch.object(usage_ledger, "datetime", _PINNED_MODULE):
    ledger = usage_ledger.UsageLedger(path)
    with ledger.batch():
      for file_path, records in files.items():
        ledger.record_file(host, file_path, "synthetic-1", records)
      ledger.mark_capture_finished(host)
    # The one-time aggregate backfill runs here, so the page reads a steady-state ledger and its read writes nothing.
    ledger.model_rows_with_native_starts()
    ledger.close()
  return {"ledger": "home/usage/ledger.sqlite3", "last_capture_at": CAPTURED_AT}
