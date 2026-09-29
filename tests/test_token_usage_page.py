"""Tests for the /token-usage page reading its rows from the usage ledger.

The ledger lives under tmp_path and ``capture_local`` is stubbed, so no test reads the
real charliebot home or scans any real log; every number on the page must come out of
the seeded ledger alone.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

import pytest
from conftest import make_page_request

from src.api import pages
from src.core.usage_ledger import RecordKind, UsageLedger, UsageRecord

CC, CODEX, OC, CB = "Claude Code", "Codex", "opencode", "charlie-bot"
CC_TS, CODEX_TS, OC_TS, CB_TS = (
    "2026-01-10T08:00:00+00:00", "2026-01-11T09:00:00+00:00", "2026-01-12T10:00:00+00:00", "2026-01-13T11:00:00+00:00")


@pytest.fixture(autouse=True)
def _fresh_single_flight():
  """Reset the single-flight holder around each test: a previous test's task belongs to a
  different event loop, and a failed capture leaves the failed task installed."""
  pages._token_usage_task = None
  yield
  pages._token_usage_task = None


def _record(
    record_id: str,
    source: str,
    model: str,
    ts: str,
    output: int,
    kind: RecordKind = RecordKind.NATIVE,
    sessions: tuple[str, ...] = (),
) -> UsageRecord:
  return UsageRecord(
      record_id=record_id,
      kind=kind,
      source=source,
      model=model,
      account="acct-a",
      ts=ts,
      in_fresh=10,
      cache_write=0,
      cache_read=5,
      output=output,
      sessions=sessions,
  )


def _seed(path: Path) -> None:
  """Seed one native record per source plus one counted-fallback Codex row."""
  with UsageLedger(path) as ledger:
    ledger.record_file("host", "/logs/cc.jsonl", "sig-cc", [_record("cc-1", CC, "claude-sonnet-4", CC_TS, 100)])
    ledger.record_file(
        "host",
        "/logs/codex.jsonl",
        "sig-codex",
        [
            _record("cx-1", CODEX, "gpt-5", CODEX_TS, 200),
            # No native record ever carries sess-pruned, so this fallback is counted.
            _record(
                "cx-fb",
                CODEX,
                "gpt-5",
                "2026-01-14T09:00:00+00:00",
                7000,
                kind=RecordKind.FALLBACK,
                sessions=("sess-pruned",))
        ])
    ledger.record_file("host", "/logs/oc.jsonl", "sig-oc", [_record("oc-1", OC, "o3", OC_TS, 300)])
    ledger.record_file("host", "/logs/cb.jsonl", "sig-cb", [_record("cb-1", CB, "claude-haiku-4", CB_TS, 400)])


def _stub_capture(monkeypatch: pytest.MonkeyPatch, ledger_path: Path, written: dict[str, int]) -> None:
  monkeypatch.setattr("src.core.usage_ledger.default_ledger_path", lambda: ledger_path)
  monkeypatch.setattr("src.core.token_tally.capture_local", lambda ledger: written)


def _data_rows(body: str) -> list[dict]:
  """The rows the page's JS feeds its charts and table from (the serialized ledger read)."""
  match = re.search(r"const DATA = (\{.*?\});", body, re.DOTALL)
  assert match, "page carries no DATA payload"
  return json.loads(match.group(1))["rows"]


async def _get_page(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> str:
  ledger_path = tmp_path / "usage" / "ledger.sqlite3"
  _seed(ledger_path)
  _stub_capture(monkeypatch, ledger_path, {"Codex": 3})
  response = await pages.token_usage_viewer(make_page_request("/token-usage"))
  assert response.status_code == 200
  return response.body.decode("utf-8")


@pytest.mark.asyncio
async def test_page_lists_seeded_models_with_output_totals(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """Every seeded model appears exactly once, with its ledger output total."""
  body = await _get_page(monkeypatch, tmp_path)
  rows = _data_rows(body)
  assert {(r["model"], r["output"]) for r in rows} == {
      ("claude-sonnet-4", 100), ("gpt-5", 7200), ("o3", 300), ("claude-haiku-4", 400)
  }


@pytest.mark.asyncio
async def test_counted_fallback_row_carries_a_lower_bound_and_native_rows_do_not(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """The mark's driver travels only on rows with counted fallback output behind them."""
  rows = {r["model"]: r for r in _data_rows(await _get_page(monkeypatch, tmp_path))}
  assert rows["gpt-5"]["fallback_output"] == 7000
  for model in ("claude-sonnet-4", "o3", "claude-haiku-4"):
    assert rows[model]["fallback_output"] == 0


@pytest.mark.asyncio
async def test_each_source_native_start_appears(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """The Retention item names every source's first native date, source beside its date."""
  body = await _get_page(monkeypatch, tmp_path)
  retention = re.search(r"<li><b>Retention:</b>(.*?)</li>", body, re.DOTALL).group(1)
  for src, date in ((CC, CC_TS[:10]), (CODEX, CODEX_TS[:10]), (OC, OC_TS[:10]), (CB, CB_TS[:10])):
    assert src in retention
    assert date in retention


@pytest.mark.asyncio
async def test_capture_failure_fails_the_request(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """A capture error propagates out of the request instead of rendering stale rows."""
  ledger_path = tmp_path / "usage" / "ledger.sqlite3"
  _seed(ledger_path)
  monkeypatch.setattr("src.core.usage_ledger.default_ledger_path", lambda: ledger_path)

  def boom(ledger: UsageLedger) -> dict[str, int]:
    raise RuntimeError("capture exploded")

  monkeypatch.setattr("src.core.token_tally.capture_local", boom)
  with pytest.raises(RuntimeError, match="capture exploded"):
    await pages.token_usage_viewer(make_page_request("/token-usage"))
