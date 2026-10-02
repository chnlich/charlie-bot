"""Tests for the /token-usage page reading its rows from the usage ledger.

The ledger lives under tmp_path and ``capture_local`` is stubbed, so no test reads the
real charliebot home or scans any real log; every number on the page must come out of
the seeded ledger alone. The row-merge tests go one step further back: they build synthetic
``LedgerRow`` s and call the page's context builder directly, with no ledger behind them
and an empty registry — the synthetic charlie-bot accounts resolve by their id prefix.
"""

from __future__ import annotations

import json
import re
import sqlite3
import time
from pathlib import Path

import pytest
from conftest import make_page_request

from src.api import pages
from src.core.timeouts import USAGE_PAGE_CAPTURE_LOCK_WAIT_SECONDS
from src.core.usage_ledger import LedgerAccount, LedgerRow, RecordKind, UsageLedger, UsageRecord

CC, CODEX, OC, CLC = "Claude Code", "Codex", "opencode", "CLC"
CB = "charlie-bot"  # the ledger's stored spelling for its own-log records
CC_TS, CODEX_TS, OC_TS, CB_TS = (
    "2026-01-10T08:00:00+00:00", "2026-01-11T09:00:00+00:00", "2026-01-12T10:00:00+00:00", "2026-01-13T11:00:00+00:00")


@pytest.fixture(autouse=True)
def _fresh_single_flight():
  """Reset the single-flight holder around each test: a previous test's task belongs to a
  different event loop."""
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
    account: str = "acct-a",
) -> UsageRecord:
  return UsageRecord(
      record_id=record_id,
      kind=kind,
      source=source,
      model=model,
      account=account,
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
    ledger.record_file(
        "host", "/logs/cb.jsonl", "sig-cb", [_record("cb-1", CB, "claude-haiku-4", CB_TS, 400, account="charlie-code-x")])


def _stub_capture(monkeypatch: pytest.MonkeyPatch, ledger_path: Path, written: dict[str, int]) -> None:
  monkeypatch.setattr("src.core.usage_ledger.default_ledger_path", lambda: ledger_path)
  monkeypatch.setattr("src.core.token_tally.capture_local", lambda ledger: written)
  monkeypatch.setattr(pages, "_backend_registry", dict)


def _data_rows(body: str) -> list[dict]:
  """The rows the page's JS feeds its charts and table from (the serialized ledger read)."""
  match = re.search(r"const DATA = (\{.*?\});", body, re.DOTALL)
  assert match, "page carries no DATA payload"
  return json.loads(match.group(1))["rows"]


def _seeded_ledger(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> Path:
  """Seed the ledger under tmp_path and point the page's ledger read at it."""
  ledger_path = tmp_path / "usage" / "ledger.sqlite3"
  _seed(ledger_path)
  monkeypatch.setattr("src.core.usage_ledger.default_ledger_path", lambda: ledger_path)
  return ledger_path


async def _get_page(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> str:
  ledger_path = _seeded_ledger(monkeypatch, tmp_path)
  _stub_capture(monkeypatch, ledger_path, {"Codex": 3, CB: 5})
  response = await pages.token_usage_viewer(make_page_request("/token-usage"))
  assert response.status_code == 200
  return response.body.decode("utf-8")


async def _request_with_failing_capture(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> Path:
  """The seeded-ledger request whose one capture raises: the request itself fails.

  Returns the ledger path so a follow-up request can prove recovery against it."""
  ledger_path = _seeded_ledger(monkeypatch, tmp_path)

  def boom(ledger: UsageLedger) -> dict[str, int]:
    raise RuntimeError("capture exploded")

  monkeypatch.setattr("src.core.token_tally.capture_local", boom)
  with pytest.raises(RuntimeError, match="capture exploded"):
    await pages.token_usage_viewer(make_page_request("/token-usage"))
  return ledger_path


def _ledger_row(
    source: str,
    model: str,
    *,
    calls: int = 1,
    in_fresh: int = 0,
    cache_write: int = 0,
    cache_read: int = 0,
    in_unsplit: int = 0,
    output: int = 0,
    fallback_output: int = 0,
    first: str = "2026-02-01",
    last: str = "2026-02-02",
    accounts: list[LedgerAccount] | None = None,
) -> LedgerRow:
  """A synthetic page row; ``total`` is the five token fields, as the ledger keeps it."""
  return LedgerRow(
      source=source,
      model=model,
      in_fresh=in_fresh,
      cache_write=cache_write,
      cache_read=cache_read,
      in_unsplit=in_unsplit,
      output=output,
      calls=calls,
      total=in_fresh + cache_write + cache_read + in_unsplit + output,
      first=first,
      last=last,
      fallback_calls=1 if fallback_output else 0,
      fallback_output=fallback_output,
      accounts=accounts or [],
  )


def _payload_rows(rows: list[LedgerRow]) -> list[dict]:
  """The serialized rows the page's JS would feed its charts and table from."""
  return json.loads(pages._token_usage_context(rows, {}, {}, 0.0, {})["payload"])["rows"]


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
  """The Retention item names every source's first native date, source beside its date;
  CLC's date is the charlie-bot span, whose native records are all CLC usage."""
  body = await _get_page(monkeypatch, tmp_path)
  retention = re.search(r"<li><b>Retention:</b>(.*?)</li>", body, re.DOTALL).group(1)
  for src, date in ((CC, CC_TS[:10]), (CODEX, CODEX_TS[:10]), (OC, OC_TS[:10]), (CLC, CB_TS[:10])):
    assert src in retention
    assert date in retention


@pytest.mark.asyncio
async def test_capture_failure_fails_the_request(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """A capture error propagates out of the request instead of rendering stale rows."""
  await _request_with_failing_capture(monkeypatch, tmp_path)


@pytest.mark.asyncio
async def test_failed_capture_clears_itself_so_the_next_request_renders(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """A failed capture uninstalls itself instead of poisoning later requests: once the
  capture works again, the very next request renders the seeded rows (no server restart)."""
  ledger_path = await _request_with_failing_capture(monkeypatch, tmp_path)

  _stub_capture(monkeypatch, ledger_path, {"Codex": 3})
  body = (await pages.token_usage_viewer(make_page_request("/token-usage"))).body.decode("utf-8")
  assert {(r["model"], r["output"]) for r in _data_rows(body)} == {
      ("claude-sonnet-4", 100), ("gpt-5", 7200), ("o3", 300), ("claude-haiku-4", 400)
  }


@pytest.mark.asyncio
async def test_locked_capture_serves_the_stored_rows(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """A capture that timed out on the ledger's write lock renders the stored rows: the lock
  says this load could not add rows, not that the stored rows are wrong."""
  _seeded_ledger(monkeypatch, tmp_path)

  def locked(ledger: UsageLedger) -> dict[str, int]:
    raise sqlite3.OperationalError("database is locked")

  monkeypatch.setattr("src.core.token_tally.capture_local", locked)
  response = await pages.token_usage_viewer(make_page_request("/token-usage"))
  assert response.status_code == 200
  assert {(r["model"], r["output"]) for r in _data_rows(response.body.decode("utf-8"))} == {
      ("claude-sonnet-4", 100), ("gpt-5", 7200), ("o3", 300), ("claude-haiku-4", 400)
  }


@pytest.mark.asyncio
async def test_capture_operational_error_other_than_lock_still_fails(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """The lock fallback stays narrow: every other sqlite error still fails the request."""
  _seeded_ledger(monkeypatch, tmp_path)

  def broken(ledger: UsageLedger) -> dict[str, int]:
    raise sqlite3.OperationalError("no such table: usage")

  monkeypatch.setattr("src.core.token_tally.capture_local", broken)
  with pytest.raises(sqlite3.OperationalError, match="no such table"):
    await pages.token_usage_viewer(make_page_request("/token-usage"))


def test_set_lock_wait_bounds_how_long_a_write_waits_out_a_lock(tmp_path: Path) -> None:
  """The page's short bound is the busy handler's ceiling: a write against a held write
  transaction raises within it instead of outlasting the request that set the bound."""
  ledger_path = tmp_path / "usage" / "ledger.sqlite3"
  with UsageLedger(ledger_path) as ledger:  # schema first: the blocker's lock must not eat the open
    blocker = sqlite3.connect(ledger_path, timeout=5)
    try:
      blocker.execute("CREATE TABLE page_probe (x)")  # uncommitted write transaction: the lock held
      blocker.execute("INSERT INTO page_probe VALUES (1)")
      ledger.set_lock_wait(USAGE_PAGE_CAPTURE_LOCK_WAIT_SECONDS)
      started = time.monotonic()
      with pytest.raises(sqlite3.OperationalError, match="database is locked"):
        ledger.record_gate("host", "/logs/x.jsonl", (1, 1), None, "sig")
      assert time.monotonic() - started < USAGE_PAGE_CAPTURE_LOCK_WAIT_SECONDS * 10
    finally:
      blocker.rollback()
      blocker.close()


def test_spellings_of_one_model_merge_into_one_row() -> None:
  """opencode's prefixed spelling and charlie-bot's bare one land on a single row: the
  display name is the largest part's spelling, the source · account sub-rows follow its
  total order and sum to the row, and the per-source segments sum to it too."""
  rows = [
      _ledger_row(OC, "zai-org/GLM-5.3-Flash", in_fresh=20, output=10, accounts=[LedgerAccount("a", 3, 10, 30)]),
      _ledger_row(CB, "GLM-5.3-Flash", output=10, accounts=[LedgerAccount("charlie-code-b", 1, 10, 10)]),
  ]
  (row,) = _payload_rows(rows)
  assert row["model"] == "GLM-5.3-Flash"
  assert row["total"] == 40
  assert [(a["name"], a["total"]) for a in row["accounts"]] == [("opencode · a", 30), ("CLC · charlie-code-b", 10)]
  assert [s["total"] for s in row["segments"]] == [30, 10]
  assert [s["slot"] for s in row["segments"]] == [pages._USAGE_SLOT[OC], pages._USAGE_SLOT[CLC]]
  assert sum(s["total"] for s in row["segments"]) == row["total"]


def test_canonical_name_merges_spellings_but_keeps_versions_apart() -> None:
  """The canonical name is the casefolded leaf minus a trailing (provider) suffix: the
  Kimi and GLM spellings each merge into one row named for their largest part, while
  claude-fable-5 and claude-fable-5-1 stay two rows."""
  rows = [
      _ledger_row(OC, "moonshotai/Kimi-K3", output=5),
      _ledger_row(OC, "Kimi-K3 (amd-kimi-k3)", output=7),
      _ledger_row(CODEX, "GLM-5.3-flash", output=3),
      _ledger_row(OC, "zai-org/GLM-5.3-Flash", output=4),
      _ledger_row(CC, "claude-fable-5", output=9),
      _ledger_row(CC, "claude-fable-5-1", output=11),
  ]
  got = sorted((r["model"], r["output"]) for r in _payload_rows(rows))
  assert got == [("GLM-5.3-Flash", 7), ("Kimi-K3", 12), ("claude-fable-5", 9), ("claude-fable-5-1", 11)]


def test_per_source_tiles_count_attributed_accounts() -> None:
  """The tiles answer how much each CLI ran, so they sum the accounts attributed to the
  source and dedupe their model count on the canonical name: opencode's three spellings
  show two tile models behind the page's two merged rows, and one charlie-bot row splits
  between the CLC and Codex tiles its accounts attribute to."""
  rows = [
      _ledger_row(OC, "moonshotai/Kimi-K3", in_fresh=5, accounts=[LedgerAccount("a", 1, 0, 5)]),
      _ledger_row(OC, "Kimi-K3 (amd-kimi-k3)", output=7, accounts=[LedgerAccount("a", 1, 7, 7)]),
      _ledger_row(OC, "zai-org/GLM-5.3-Flash", output=4, accounts=[LedgerAccount("a", 1, 4, 4)]),
      _ledger_row(
          CB,
          "GLM-5.3-Flash",
          output=3,
          accounts=[LedgerAccount("charlie-code-x", 1, 2, 2), LedgerAccount("codex-y", 1, 1, 1)]),
  ]
  ctx = pages._token_usage_context(rows, {}, {}, 0.0, {})["ctx"]
  assert ctx["per_src"][OC]["models"] == 2
  assert ctx["per_src"][OC]["total"] == 16
  assert ctx["per_src"][CLC]["t_comp"] == pages._compact(2)
  assert ctx["per_src"][CLC]["models"] == 1
  assert ctx["per_src"][CODEX]["total"] == 1
  assert ctx["per_src"][CC]["total"] == 0


def test_in_unsplit_carries_into_the_payload_the_table_column_and_the_hero() -> None:
  """The unsplit input rides the payload row that feeds the table column, and the hero's
  total and input figures count it like the other input columns."""
  rows = [_ledger_row(CC, "claude-sonnet-4", in_fresh=7, cache_write=3, cache_read=11, in_unsplit=5, output=4)]
  ctx = pages._token_usage_context(rows, {}, {}, 0.0, {})["ctx"]
  (row,) = _payload_rows(rows)
  assert row["in_unsplit"] == 5
  assert row["total"] == 30
  assert ctx["tot_compact"] == pages._compact(30)
  assert ctx["in_compact"] == pages._compact(26)


@pytest.mark.asyncio
async def test_rendered_page_has_one_row_per_canonical_model(monkeypatch: pytest.MonkeyPatch) -> None:
  """End to end over the real template: two sources' spellings of one model render one
  page row, carrying the merged source · account sub-rows and both sources' segments."""
  rows = [
      _ledger_row(OC, "zai-org/GLM-5.3-Flash", in_fresh=20, output=10, accounts=[LedgerAccount("a", 3, 10, 30)]),
      _ledger_row(CB, "GLM-5.3-Flash", output=10, accounts=[LedgerAccount("charlie-code-b", 1, 10, 10)]),
      _ledger_row(CC, "claude-sonnet-4", output=100),
  ]
  monkeypatch.setattr(pages, "_capture_ledger_rows", lambda: (rows, {}, {}, 0.0, {}))
  response = await pages.token_usage_viewer(make_page_request("/token-usage"))
  assert response.status_code == 200
  data = _data_rows(response.body.decode("utf-8"))
  assert [(r["model"], r["total"]) for r in data] == [("claude-sonnet-4", 100), ("GLM-5.3-Flash", 40)]
  glm = data[1]
  assert [a["name"] for a in glm["accounts"]] == ["opencode · a", "CLC · charlie-code-b"]
  assert [s["total"] for s in glm["segments"]] == [30, 10]


def test_charlie_bot_native_accounts_read_as_clc() -> None:
  """A charlie-bot row's CLC-backend account attributes to CLC: one CLC segment, the
  CLC · account sub-row, the CLC tile — and CLC's native start is the ledger's charlie-bot
  native start, whose native records are all CLC usage."""
  rows = [_ledger_row(CB, "glm-z", in_unsplit=100, output=40, accounts=[LedgerAccount("charlie-code-x", 2, 40, 140)])]
  (row,) = _payload_rows(rows)
  assert [(s["slot"], s["total"]) for s in row["segments"]] == [(pages._USAGE_SLOT[CLC], 140)]
  assert [a["name"] for a in row["accounts"]] == ["CLC · charlie-code-x"]
  ctx = pages._token_usage_context(rows, {CB: "2026-01-01"}, {}, 0.0, {})["ctx"]
  assert ctx["per_src"][CLC]["t_comp"] == pages._compact(140)
  assert ctx["per_src"][CLC]["native_start"] == "2026-01-01"


def test_charlie_bot_fallback_accounts_join_their_cli_s_source() -> None:
  """A charlie-bot row's fallback account merges into the CLI that ran it: model gpt-z's
  Codex row and charlie-bot fallback row give one row of 40 with a single Codex segment,
  the CLI's own account unmarked and the fallback account's sub-row marked, and a Codex
  tile of 40."""
  rows = [
      _ledger_row(CODEX, "gpt-z", output=30, accounts=[LedgerAccount("work", 3, 30, 30)]),
      _ledger_row(CB, "gpt-z", output=10, fallback_output=10, accounts=[LedgerAccount("codex-y", 1, 10, 10)]),
  ]
  (row,) = _payload_rows(rows)
  assert row["total"] == 40
  assert [(s["slot"], s["total"]) for s in row["segments"]] == [(pages._USAGE_SLOT[CODEX], 40)]
  assert [(a["name"], a["total"]) for a in row["accounts"]] == [
      ("Codex · work", 30),
      ("Codex · codex-y (fallback)", 10),
  ]
  ctx = pages._token_usage_context(rows, {}, {}, 0.0, {})["ctx"]
  assert ctx["per_src"][CODEX]["total"] == 40
  assert ctx["per_src"][CLC]["total"] == 0


def test_four_tile_totals_sum_to_the_page_total() -> None:
  """Every token lands in exactly one tile: the four attributed tile totals sum to the
  page total across all rows."""
  rows = [
      _ledger_row(CC, "claude-sonnet-4", in_fresh=7, output=4, accounts=[LedgerAccount("acct-a", 1, 4, 11)]),
      _ledger_row(CODEX, "gpt-z", output=30, accounts=[LedgerAccount("work", 3, 30, 30)]),
      _ledger_row(CB, "gpt-z", output=10, accounts=[LedgerAccount("codex-y", 1, 10, 10)]),
      _ledger_row(CB, "glm-z", in_unsplit=100, output=40, accounts=[LedgerAccount("charlie-code-x", 2, 40, 140)]),
      _ledger_row(OC, "o3", output=300, accounts=[LedgerAccount("acct-a", 3, 300, 300)]),
  ]
  ctx = pages._token_usage_context(rows, {}, {}, 0.0, {})["ctx"]
  assert sum(ctx["per_src"][src]["total"] for src in (CC, CODEX, OC, CLC)) == sum(r.total for r in rows)


@pytest.mark.asyncio
async def test_page_legend_names_the_four_cli_sources_without_charlie_bot(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  """The rendered legend lists Claude Code, Codex, opencode, CLC in slot order, the
  self-check names CharlieBot's own log, and no charlie-bot text reaches the page."""
  body = await _get_page(monkeypatch, tmp_path)
  leg = re.search(r"const LEG = (\[[^\]]*\])", body).group(1)
  assert json.loads(leg) == [CC, CODEX, OC, CLC]
  assert "CharlieBot logs: 5 records written this load" in body
  assert "charlie-bot" not in body
