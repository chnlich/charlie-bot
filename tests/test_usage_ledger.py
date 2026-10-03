"""Tests for the SQLite usage ledger (src/core/usage_ledger.py).

Every id, path, host and name here is synthetic and each ledger lives under tmp_path,
so no test reads the real charliebot home or any captured file. Each assertion checks
a named mechanism (dedup, exclusion, order independence, upsert stability) rather
than a hard-coded total.
"""

from __future__ import annotations

import sqlite3
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.core.usage_ledger import (
    _MODEL_ROWS_SQL,
    _SCHEMA,
    LedgerRow,
    RecordKind,
    UsageLedger,
    UsageRecord,
    _fold_grouped_row,
    _rows_from_accs,
    default_ledger_path,
)

SOURCE = "src-a"
HOST = "host-a"
TS_A = "2026-01-10T08:00:00+00:00"
TS_B = "2026-01-11T09:30:00+00:00"


def _record(
    record_id: str,
    kind: RecordKind,
    sessions: tuple[str, ...] = (),
    *,
    model: str = "model-a",
    account: str = "acct-a",
    ts: str = TS_A,
    output: int = 5,
    in_unsplit: int = 0,
) -> UsageRecord:
  return UsageRecord(
      record_id=record_id,
      kind=kind,
      source=SOURCE,
      model=model,
      account=account,
      ts=ts,
      in_fresh=10,
      cache_write=2,
      cache_read=3,
      output=output,
      in_unsplit=in_unsplit,
      sessions=sessions,
  )


def test_one_record_id_from_two_hosts_counts_once(tmp_path):
  rec = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert ledger.record_file("host-a", "/logs/a.jsonl", "sig-a", [rec]) == 1
    assert ledger.record_file("host-b", "/logs/b.jsonl", "sig-b", [rec]) == 1
    rows = ledger.model_rows()
  assert len(rows) == 1
  assert rows[0].calls == 1
  assert rows[0].output == 5
  assert rows[0].total == 20


def test_fallback_excluded_once_any_session_is_native(tmp_path):
  fb = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-a", "sess-b"), model="model-fb", ts=TS_B)
  native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-a",), model="model-native")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb", [fb])
    rows = ledger.model_rows()
    assert {row.model for row in rows} == {"model-fb"}
    assert rows[0].fallback_calls == 1
    assert rows[0].fallback_output == 5
    ledger.record_file(HOST, "/logs/native.jsonl", "sig-native", [native])
    models = {row.model for row in ledger.model_rows()}
  assert models == {"model-native"}


def test_rows_equal_for_either_write_order(tmp_path):
  native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-a",), account="acct-a")
  fb = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-b",), account="acct-b", ts=TS_B, output=7)
  with UsageLedger(tmp_path / "native-first.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/native.jsonl", "sig-n", [native])
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-f", [fb])
    native_first = ledger.model_rows()
  with UsageLedger(tmp_path / "fallback-first.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-f", [fb])
    ledger.record_file(HOST, "/logs/native.jsonl", "sig-n", [native])
    fallback_first = ledger.model_rows()
  assert native_first == fallback_first
  assert len(native_first) == 1
  row = native_first[0]
  assert row.calls == 2
  assert row.total == 42
  assert row.first == "2026-01-10"
  assert row.last == "2026-01-11"
  assert row.fallback_calls == 1
  assert row.fallback_output == 7
  # The fallback account carries more tokens, so it leads despite the later capture.
  assert [(a.name, a.total) for a in row.accounts] == [("acct-b", 22), ("acct-a", 20)]


def test_empty_ts_never_becomes_first_or_last(tmp_path):
  empty = _record("rec-empty", RecordKind.NATIVE, sessions=("sess-e",), ts="")
  early = _record("rec-early", RecordKind.NATIVE, sessions=("sess-a",), ts=TS_A, account="acct-b")
  late = _record("rec-late", RecordKind.NATIVE, sessions=("sess-b",), ts=TS_B)
  blank = _record("rec-blank", RecordKind.NATIVE, sessions=("sess-c",), ts="", model="model-blank")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [empty, early, late, blank])
    rows = {row.model: row for row in ledger.model_rows()}
  # The empty-ts record still counts, but never anchors the span: the dated ones do.
  assert rows["model-a"].calls == 3
  assert rows["model-a"].first == "2026-01-10"
  assert rows["model-a"].last == "2026-01-11"
  # No dated record in the group: no day to anchor, first/last stay empty.
  assert (rows["model-blank"].first, rows["model-blank"].last) == ("", "")


def test_reupserted_native_keeps_first_session_registered(tmp_path):
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",))
  second = _record("rec-1", RecordKind.NATIVE, sessions=("sess-b",))
  fb = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-a",), model="model-fb", ts=TS_B)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/one.jsonl", "sig-1", [first])
    ledger.record_file(HOST, "/logs/two.jsonl", "sig-2", [second])
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb", [fb])
    models = {row.model for row in ledger.model_rows()}
  # The upsert replaced the native row but only added a session: sess-a stays
  # registered, so the fallback carrying it is still presumed restated.
  assert models == {"model-a"}


def test_one_file_many_records_share_one_session_link(tmp_path):
  """A transcript file's records all register the same (session, source) pair: the
  batched links land one native_sessions row while every record still counts once."""
  recs = [_record(f"rec-{i}", RecordKind.NATIVE, sessions=("sess-a",)) for i in range(50)]
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", recs) == 50
    links = ledger._conn.execute("SELECT session, source FROM native_sessions").fetchall()
    usage_count = ledger._conn.execute("SELECT count(*) FROM usage").fetchone()[0]
    rows = ledger.model_rows()
  assert [(row["session"], row["source"]) for row in links] == [("sess-a", SOURCE)]
  assert usage_count == 50
  assert rows[0].calls == 50


def test_mixed_kind_file_sharing_one_session_keeps_the_fallback_excluded(tmp_path):
  """One file carrying a fallback record and a native record for the same session nets
  the fallback excluded: its usage row and link persist, the aggregate counts only the
  native record -- the same end state the per-record write order reached by counting
  the fallback and retiring it at the later native link."""
  fb = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-a",), model="model-fb", ts=TS_B)
  native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-a",), model="model-native")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/m.jsonl", "sig-m", [fb, native])
    models = {row.model for row in ledger.model_rows()}
    usage_ids = {row[0] for row in ledger._conn.execute("SELECT record_id FROM usage")}
    fb_links = ledger._conn.execute("SELECT count(*) FROM fallback_sessions WHERE record_id = 'rec-fb'").fetchone()[0]
  assert models == {"model-native"}
  assert usage_ids == {"rec-fb", "rec-native"}
  assert fb_links == 1


def test_relinked_counted_fallback_in_a_mixed_batch_is_retired_by_the_later_native_link(tmp_path):
  """A counted fallback record that gains a session in the same mixed-kind batch where a
  native record registers that session nets the fallback retired: its link lands before
  the native link, whose retire trigger subtracts the count. The per-record order is what
  reaches that scan -- links-first would strand the count on the aggregate behind the
  first-session guard."""
  fb = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-a",), model="model-fb")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-1", [fb])
    counted = ledger.model_rows()
    assert [row.fallback_calls for row in counted] == [1]
    grown = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-a", "sess-b"), model="model-fb")
    native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-b",), model="model-native")
    ledger.record_file(HOST, "/logs/mixed.jsonl", "sig-2", [grown, native])
    models = {row.model: row for row in ledger.model_rows()}
  assert set(models) == {"model-native"}
  assert models["model-native"].calls == 1


def test_rewrite_identical_records_leaves_rows_unchanged(tmp_path):
  recs = [
      _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",)),
      _record("rec-2", RecordKind.FALLBACK, sessions=("sess-b",), ts=TS_B),
  ]
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", recs)
    before = ledger.model_rows()
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", recs)
    after = ledger.model_rows()
    sigs = ledger.captured_sigs(HOST)
  assert before == after
  assert sigs == {"/logs/a.jsonl": "sig-a"}


def test_captured_sigs_returns_latest_sig_per_path(tmp_path):
  rec = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file("host-a", "/logs/a.jsonl", "sig-old", [rec])
    ledger.record_file("host-a", "/logs/a.jsonl", "sig-new", [rec])
    ledger.record_file("host-a", "/logs/b.jsonl", "sig-b", [rec])
    sigs = ledger.captured_sigs("host-a")
  assert sigs == {"/logs/a.jsonl": "sig-new", "/logs/b.jsonl": "sig-b"}


def test_fallback_without_sessions_rejected():
  with pytest.raises(ValueError):
    _record("rec-fb", RecordKind.FALLBACK, sessions=())


def test_unknown_stored_kind_raises_on_model_rows(tmp_path):
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger._conn.execute(
        "INSERT INTO usage (record_id, kind, source, model, account, host, ts,"
        " in_fresh, cache_write, cache_read, output, origin, captured_at)"
        " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ("rec-x", "replayed", SOURCE, "model-x", "acct-a", HOST, TS_A, 1, 0, 0, 2, "/logs/x.jsonl", TS_A))
    ledger._conn.commit()
    with pytest.raises(ValueError):
      ledger.model_rows()


def test_native_start_ignores_fallback_only_spans(tmp_path):
  native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-a",), ts=TS_B)
  # A fallback for the same source seen earlier must not pull the start back: its
  # span rides on a prunable log, the native one does not.
  fb = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-b",), ts=TS_A)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb", [fb])
    ledger.record_file(HOST, "/logs/native.jsonl", "sig-n", [native])
    rows, starts = ledger.model_rows_with_native_starts()
  assert starts == {SOURCE: "2026-01-11"}
  assert [row.model for row in rows] == ["model-a"]


def test_native_start_survives_an_empty_ts_native_row(tmp_path):
  """One empty-ts native row must not MIN the source's start to the empty string."""
  undated = _record("rec-undated", RecordKind.NATIVE, sessions=("sess-u",), ts="")
  dated = _record("rec-dated", RecordKind.NATIVE, sessions=("sess-d",), ts=TS_B)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/u.jsonl", "sig-u", [undated])
    ledger.record_file(HOST, "/logs/d.jsonl", "sig-d", [dated])
    _rows, starts = ledger.model_rows_with_native_starts()
  assert starts == {SOURCE: "2026-01-11"}


def _table_pass_rows(ledger: UsageLedger) -> list[LedgerRow]:
  """The page rows folded straight from the table pass -- the read's ground truth."""
  accs = {}
  for row in ledger._conn.execute(_MODEL_ROWS_SQL):
    _fold_grouped_row(accs, row)
  return _rows_from_accs(accs)


def _agg_rows(ledger: UsageLedger) -> list[LedgerRow]:
  """The page rows folded from the trigger-maintained aggregate -- what a served read
  prices once the backfill has run, read here without the memo in the way."""
  accs = ledger._agg_accs()
  assert accs is not None  # the aggregate has been backfilled and serves
  return _rows_from_accs(accs)


def test_agg_read_tracks_the_table_through_every_write_shape(tmp_path):
  """The served rows equal the table pass after each shape the write path produces:
  a cross-day rewrite, a same-group-day insert, a counted fallback retired by a native
  session, a fallback born excluded, an excluded row rewritten, an empty-ts row, and a
  foreign row delete (the module never deletes, but a raw delete must not leave the
  aggregate behind). Every step checks both served paths: model_rows -- memo, fold, or
  aggregate -- and the trigger-maintained aggregate itself."""
  moved = _record("rec-moved", RecordKind.NATIVE, sessions=("sess-m",), ts=TS_A, output=5)
  fb = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-x",), model="model-fb", ts=TS_B, output=7)
  native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-x",), model="model-native")
  empty = _record("rec-empty", RecordKind.NATIVE, sessions=("sess-e",), ts="", model="model-empty")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:

    def assert_served_on_the_table() -> None:
      assert ledger.model_rows() == _table_pass_rows(ledger)
      assert _agg_rows(ledger) == _table_pass_rows(ledger)

    ledger.record_file(HOST, "/logs/m.jsonl", "sig-m", [moved, fb, empty])
    ledger.model_rows()  # the backfill; the aggregate serves from here
    assert_served_on_the_table()
    moved_grown = _record("rec-moved", RecordKind.NATIVE, sessions=("sess-m",), ts=TS_B, output=9)
    ledger.record_file(HOST, "/logs/m.jsonl", "sig-m2", [moved_grown])
    assert_served_on_the_table()
    same_day = _record("rec-same-day", RecordKind.NATIVE, sessions=("sess-sd",), ts=TS_B, output=6)
    ledger.record_file(HOST, "/logs/sd.jsonl", "sig-sd", [same_day])  # joins moved_grown's group-day
    assert_served_on_the_table()
    ledger.record_file(HOST, "/logs/n.jsonl", "sig-n", [native])  # retires rec-fb
    assert_served_on_the_table()
    born_excluded = _record("rec-be", RecordKind.FALLBACK, sessions=("sess-x",), model="model-be", ts=TS_B)
    ledger.record_file(HOST, "/logs/be.jsonl", "sig-be", [born_excluded])
    assert_served_on_the_table()
    rewritten_excluded = _record(
        "rec-fb", RecordKind.FALLBACK, sessions=("sess-x",), model="model-fb", ts=TS_B, output=50)
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb2", [rewritten_excluded])
    assert_served_on_the_table()
    ledger._conn.execute("DELETE FROM usage WHERE record_id = 'rec-moved'")
    ledger._conn.commit()
    assert_served_on_the_table()


def test_previous_release_write_order_never_counts_a_born_excluded_fallback(tmp_path):
  """The previous release upserted the usage row before registering sessions, so on a
  triggered ledger the insert trigger saw none and counted a born-excluded fallback.
  The first-session trigger subtracts it; this release's own order (sessions first)
  reaches that trigger before the usage row exists, where it no-ops."""
  native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-a",), model="model-native")
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/native.jsonl", "sig-native", [native])
    expected = ledger.model_rows()  # the backfill runs, the aggregate serves from here
    # The previous release's order, written through raw SQL exactly as its record_file did:
    conn = sqlite3.connect(path)
    try:
      conn.execute(
          "INSERT INTO usage (record_id, kind, source, model, account, host, ts,"
          " in_fresh, cache_write, cache_read, output, origin, captured_at)"
          " VALUES ('rec-fb', 'fallback', ?, 'model-fb', 'acct-a', ?, ?, 10, 2, 3, 7, '/x', ?)",
          (SOURCE, HOST, TS_B, TS_B))
      conn.execute("INSERT INTO fallback_sessions (record_id, session) VALUES ('rec-fb', 'sess-a')")
      conn.commit()
    finally:
      conn.close()
    rows = ledger.model_rows()
  assert [row.model for row in expected] == ["model-native"]
  assert [row.model for row in rows] == ["model-native"]


def test_backfill_prices_a_ledger_written_before_the_aggregate(tmp_path):
  """Rows written before the triggers existed (any pre-aggregate writer's shape) price
  the first new-code read exactly: the backfill folds the whole table once."""
  rec = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",))
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [rec])
  # Strip the aggregate to the pre-trigger shape: no table, no flag, and rows a raw
  # writer (no triggers) adds afterwards.
  raw = sqlite3.connect(path)
  try:
    raw.executescript("DROP TABLE usage_agg; DELETE FROM ledger_meta; DROP TRIGGER usage_agg_after_insert;")
    raw.execute(
        "INSERT INTO usage (record_id, kind, source, model, account, host, ts,"
        " in_fresh, cache_write, cache_read, output, origin, captured_at)"
        " VALUES ('rec-2', 'native', ?, 'model-b', 'acct-a', ?, ?, 4, 1, 1, 3, '/x', ?)", (SOURCE, HOST, TS_B, TS_B))
    raw.commit()
  finally:
    raw.close()
  with UsageLedger(path) as ledger:
    rows = ledger.model_rows()
  assert [row.model for row in rows] == ["model-a", "model-b"]
  assert rows[0].calls == 1 and rows[1].calls == 1


def test_backfilled_read_serves_the_aggregate_not_the_table_pass(tmp_path):
  """Once backfilled, a read the memo and fold miss prices the aggregate: the table
  pass never runs, which is the term the page's wall sheds."""
  rec = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [rec])
    ledger.model_rows()  # first read runs the backfill
    expected = _table_pass_rows(ledger)  # the ground truth, traced or not
    ran: list[str] = []
    ledger._conn.set_trace_callback(ran.append)
    try:
      rows = ledger.model_rows()
    finally:
      ledger._conn.set_trace_callback(None)
    assert rows == expected
    table_passes = [s for s in ran if "FROM usage u" in s and "usage_agg" not in s]
    assert table_passes == []


def test_dropped_aggregate_table_reprices_on_the_next_read(tmp_path):
  """A ready flag over a dropped aggregate table (no statement here drops one) must not
  serve an empty page: the read re-prices and the rows survive."""
  rec = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",))
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [rec])
    expected = ledger.model_rows()
    ledger._conn.execute("DROP TABLE usage_agg")
    ledger._conn.commit()
  with UsageLedger(path) as ledger:  # the reopen's schema script rebuilds the table
    rows = ledger.model_rows()
  assert rows == expected


def test_backfilled_aggregate_counts_later_same_group_day_writes(tmp_path):
  """After the backfill, inserts onto an existing (source, model, account, kind, day) row
  increment the aggregate: two more records sharing the first's group-day both count,
  read through _agg_accs -- the path the page read serves -- not the memo."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), output=6)
  third = _record("rec-3", RecordKind.NATIVE, sessions=("sess-c",), output=7)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    ledger.model_rows()  # the backfill; the aggregate serves from here
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", [second, third])
    served = _agg_rows(ledger)
    assert served == _table_pass_rows(ledger)
  assert [(row.calls, row.output) for row in served] == [(3, 18)]


def test_rewrite_within_a_shared_group_day_keeps_the_aggregate_on_the_table(tmp_path):
  """Rewriting one of several records sharing a (group, day) row subtracts the old
  contribution and re-adds the new one, so the served aggregate stays the table pass."""
  records = [
      _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5),
      _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), output=5),
      _record("rec-3", RecordKind.NATIVE, sessions=("sess-c",), output=5),
  ]
  rewritten = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), output=50)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", records)
    ledger.model_rows()  # the backfill
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", [rewritten])
    served = _agg_rows(ledger)
    assert served == _table_pass_rows(ledger)
  assert [(row.calls, row.output) for row in served] == [(3, 60)]


def test_default_ledger_path_derives_from_the_config_home(monkeypatch, tmp_path):
  """The CLI's default ledger resolves per call from the config's charliebot home."""
  import src.core.config as config_module

  monkeypatch.setattr(config_module, "get_config", lambda: SimpleNamespace(charliebot_home=tmp_path / "home"))
  assert default_ledger_path() == tmp_path / "home" / "usage" / "ledger.sqlite3"


def test_ledger_without_index_gains_it_on_reopen(tmp_path):
  """A ledger file created before the cover index gains it on its next open."""
  rec = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",))
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [rec])
    before = ledger.model_rows()
    ledger._conn.execute("DROP INDEX usage_group_cover")
    ledger._conn.commit()
  with UsageLedger(path) as ledger:
    names = {row["name"] for row in ledger._conn.execute("SELECT name FROM sqlite_master WHERE type = 'index'")}
    assert "usage_group_cover" in names
    assert ledger.model_rows() == before


def test_model_rows_plan_scans_the_cover_index_without_analyze(tmp_path):
  """The counted-rows query scans usage_group_cover outright on a fresh ledger.

  No ANALYZE has ever run here (sqlite_stat1 absent), so the OR-free NOT EXISTS form
  must be what keeps the planner off the MULTI-INDEX OR + temp-B-tree plan.
  """
  native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-a",))
  kept = _record("rec-fb-kept", RecordKind.FALLBACK, sessions=("sess-b",), model="model-kept")
  excluded = _record("rec-fb-excluded", RecordKind.FALLBACK, sessions=("sess-a",), model="model-excluded", ts=TS_B)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [native, kept, excluded])
    assert not ledger._conn.execute("SELECT 1 FROM sqlite_master WHERE name = 'sqlite_stat1'").fetchone()
    plan = [row["detail"] for row in ledger._conn.execute("EXPLAIN QUERY PLAN " + _MODEL_ROWS_SQL)]
  assert any("SCAN" in detail and "usage_group_cover" in detail for detail in plan)
  assert not any("MULTI-INDEX OR" in detail for detail in plan)


def test_capture_gate_round_trip(tmp_path) -> None:
  """The gate stores the file-state pairs a probe ran under and the signature it computed;
  a re-record replaces both, and a path with no gate reads None."""
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert ledger.captured_gate(HOST, "/data/db.sqlite") is None
    ledger.record_gate(HOST, "/data/db.sqlite", (10, 20), (30, 40), "sig-1")
    assert ledger.captured_gate(HOST, "/data/db.sqlite") == (((10, 20), (30, 40)), "sig-1")
    ledger.record_gate(HOST, "/data/db.sqlite", (11, 21), None, "sig-2")
    assert ledger.captured_gate(HOST, "/data/db.sqlite") == (((11, 21), None), "sig-2")
    assert ledger.captured_gate("other-host", "/data/db.sqlite") is None


def test_reopen_recreates_a_dropped_gate_table(tmp_path) -> None:
  """The gate table is created on open (IF NOT EXISTS), so a ledger written before the
  table existed upgrades on its first open with no migration step."""
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_gate(HOST, "/data/db.sqlite", (10, 20), None, "sig-1")
  con = sqlite3.connect(path)
  try:
    con.execute("DROP TABLE capture_gates")
    con.commit()
  finally:
    con.close()
  with UsageLedger(path) as ledger:
    assert ledger.captured_gate(HOST, "/data/db.sqlite") is None
    ledger.record_gate(HOST, "/data/db.sqlite", (10, 20), None, "sig-1")
    assert ledger.captured_gate(HOST, "/data/db.sqlite") == (((10, 20), None), "sig-1")


def test_rows_read_serves_the_memo_while_the_file_sits_still(tmp_path):
  """A repeat read with no writer in between returns the memoized row objects."""
  rec = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [rec])
    rows, starts = ledger.model_rows_with_native_starts()
    rows_again, starts_again = ledger.model_rows_with_native_starts()
  assert rows_again is rows
  assert starts_again is starts


def test_rows_read_reflects_a_write_from_another_instance(tmp_path):
  """A writer this process never saw (another instance, as another process) invalidates the memo."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    rows_before, _ = ledger.model_rows_with_native_starts()
  with UsageLedger(path) as writer:
    writer.record_file(HOST, "/logs/b.jsonl", "sig-b", [second])
  with UsageLedger(path) as ledger:
    rows_after, _ = ledger.model_rows_with_native_starts()
  assert [row.model for row in rows_before] == ["model-a"]
  assert [row.model for row in rows_after] == ["model-b", "model-a"]


def test_rows_read_reflects_this_instances_own_write(tmp_path):
  """The capture writes through the same connection it reads from; the memo must not hide it."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    rows_before, _ = ledger.model_rows_with_native_starts()
    ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", [second])
    rows_after, _ = ledger.model_rows_with_native_starts()
  assert [row.model for row in rows_before] == ["model-a"]
  assert [row.model for row in rows_after] == ["model-b", "model-a"]


def test_wal_ledger_never_serves_the_memo(tmp_path):
  """Under WAL a commit hides in the -wal sidecar, so the stat pair cannot witness it:
  the memo stores nothing and every read re-runs the grouped pass."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger._conn.execute("PRAGMA journal_mode=wal")
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    rows_before, _ = ledger.model_rows_with_native_starts()
    rows_again, _ = ledger.model_rows_with_native_starts()
    assert rows_again is not rows_before
  with UsageLedger(path) as writer:
    writer._conn.execute("PRAGMA journal_mode=wal")
    writer.record_file(HOST, "/logs/b.jsonl", "sig-b", [second])
  with UsageLedger(path) as ledger:
    rows_after, _ = ledger.model_rows_with_native_starts()
  assert [row.model for row in rows_after] == ["model-b", "model-a"]


def _read_path(monkeypatch, ledger) -> list[str]:
  """Record which read path each model_rows call takes while wrapped."""
  paths: list[str] = []
  original = UsageLedger._try_fold_rows

  def spy(self, memo, identity, generation):
    folded = original(self, memo, identity, generation)
    paths.append("fold" if folded is not None else "full")
    return folded

  monkeypatch.setattr(UsageLedger, "_try_fold_rows", spy)
  assert ledger  # the spy installs per ledger; callers pass the one under test
  return paths


def test_fold_serves_rows_a_fresh_ledger_matches(tmp_path):
  """A read after this process's own inserts folds the delta into rows a fresh ledger matches."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7, ts=TS_B)
  third = _record("rec-3", RecordKind.FALLBACK, sessions=("sess-c",), model="model-c", output=9, ts=TS_B)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    ledger.model_rows_with_native_starts()
    ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", [second, third])
    rows, starts = ledger.model_rows_with_native_starts()
  with UsageLedger(path) as fresh:
    assert rows == fresh.model_rows_with_native_starts()[0]
    assert starts == fresh.model_rows_with_native_starts()[1]


def test_fold_stands_down_when_a_new_native_session_retires_a_fallback(monkeypatch, tmp_path):
  """A new native session retires a counted fallback row; the fold cannot patch the
  touched group incrementally, so it stands down and the full pass prices the retirement."""
  fb = _record("rec-fb", RecordKind.FALLBACK, sessions=("sess-a",), model="model-fb", ts=TS_B)
  native = _record("rec-native", RecordKind.NATIVE, sessions=("sess-a",), model="model-native")
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/fb.jsonl", "sig-fb", [fb])
    rows_before, _ = ledger.model_rows_with_native_starts()
    paths = _read_path(monkeypatch, ledger)
    ledger.record_file(HOST, "/logs/native.jsonl", "sig-native", [native])
    rows_after, _ = ledger.model_rows_with_native_starts()
  assert [row.model for row in rows_before] == ["model-fb"]
  assert paths == ["full"]
  assert [row.model for row in rows_after] == ["model-native"]


def test_fold_stands_down_on_another_instances_inserts(monkeypatch, tmp_path):
  """Inserts this instance did not track (another ledger object, as another worker)
  are invisible to its delta; the row-count witness refuses the fold."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7, ts=TS_B)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    ledger.model_rows_with_native_starts()
  with UsageLedger(path) as writer:
    writer.record_file(HOST, "/logs/b.jsonl", "sig-b", [second])
  with UsageLedger(path) as reader:
    paths = _read_path(monkeypatch, reader)
    rows, _ = reader.model_rows_with_native_starts()
  assert paths == ["full"]
  assert [row.model for row in rows] == ["model-b", "model-a"]


def test_identical_reupsert_keeps_the_fold(monkeypatch, tmp_path):
  """A moved file re-upserts its records with unchanged values; the fold still serves the read."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7, ts=TS_B)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    ledger.model_rows_with_native_starts()
    paths = _read_path(monkeypatch, ledger)
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", [first])
    ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", [second])
    rows, _ = ledger.model_rows_with_native_starts()
  assert paths == ["fold"]
  with UsageLedger(path) as fresh:
    assert rows == fresh.model_rows_with_native_starts()[0]


def test_value_rewrite_falls_back_to_the_full_pass(monkeypatch, tmp_path):
  """An upsert that changes an existing row's values poisons the fold; the read re-aggregates."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  rewritten = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=9)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    ledger.model_rows_with_native_starts()
    paths = _read_path(monkeypatch, ledger)
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", [rewritten])
    rows, _ = ledger.model_rows_with_native_starts()
  assert paths == ["full"]
  assert rows[0].output == 9


def test_foreign_row_delete_falls_back_to_the_full_pass(monkeypatch, tmp_path):
  """Rows that vanish under the memo (no statement here deletes) abort the fold."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7, ts=TS_B)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    ledger.model_rows_with_native_starts()
    paths = _read_path(monkeypatch, ledger)
    ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", [second])
    ledger._conn.execute("DELETE FROM usage WHERE record_id = 'rec-1'")
    ledger._conn.commit()
    rows, _ = ledger.model_rows_with_native_starts()
  assert paths == ["full"]
  assert [row.model for row in rows] == ["model-b"]


def test_fold_stands_down_on_a_foreign_value_rewrite(monkeypatch, tmp_path):
  """A foreign writer's in-place value rewrite (no row-count change) must poison the
  memo this process built: the epoch lives in the database, so the fold's next read
  sees it and the full pass re-prices the page."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  rewritten = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=107)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7, ts=TS_B)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    rows_before, _ = ledger.model_rows_with_native_starts()
  with UsageLedger(path) as foreign:  # another process's shape: its own connection
    foreign.record_file(HOST, "/logs/a.jsonl", "sig-a2", [rewritten])
  with UsageLedger(path) as reader:
    reader.record_file(HOST, "/logs/b.jsonl", "sig-b", [second])
    paths = _read_path(monkeypatch, reader)
    rows, _ = reader.model_rows_with_native_starts()
  assert [row.output for row in rows_before if row.model == "model-a"] == [5]
  assert paths == ["full"]
  assert [row.output for row in rows if row.model == "model-a"] == [107]


# --- supersede_prefix: the one bounded deletion path, and the ts-keeping conflict rule ------


def test_supersede_prefix_deletes_only_its_range_while_writing_replacements(tmp_path):
  """supersede_prefix deletes the usage rows and their fallback_sessions rows under the
  prefix -- by key range, so ids sorting past the bound survive -- in the same call that
  writes the replacement records, and the trigger-maintained aggregate tracks the
  exchange."""
  legacy = [
      _record("codex:t1:0", RecordKind.NATIVE, sessions=("sess-a",), output=5),
      _record("codex:t1:1", RecordKind.FALLBACK, sessions=("sess-b",), model="model-fb", ts=TS_B, output=7),
      _record("codex:t10:0", RecordKind.NATIVE, sessions=("sess-a",), model="model-x", output=3),
      _record("codex:t1x:0", RecordKind.NATIVE, sessions=("sess-a",), model="model-y", output=4),
      _record("codex-total:t1:5:0:2", RecordKind.NATIVE, sessions=("sess-a",), model="model-t", output=9),
  ]
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", legacy)
    replacement = _record("codex-total:t1:7:0:0", RecordKind.NATIVE, sessions=("sess-a",), model="model-t", output=6)
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", [replacement], supersede_prefix="codex:t1:")
    stored = [row["record_id"] for row in ledger._conn.execute("SELECT record_id FROM usage ORDER BY record_id")]
    links = [row["record_id"] for row in ledger._conn.execute("SELECT record_id FROM fallback_sessions")]
    assert ledger.model_rows() == _table_pass_rows(ledger)
  assert stored == ["codex-total:t1:5:0:2", "codex-total:t1:7:0:0", "codex:t10:0", "codex:t1x:0"]
  assert links == []


def test_superseding_n_rows_for_n_new_rows_keeps_every_read_layer_agreed(monkeypatch, tmp_path):
  """N deletes paired with N inserts is the shape the fold's row-count witness cannot
  see, so the supersede bumps the epoch: the fold stands down, the full pass re-prices,
  and the memo-served repeat read, the aggregate read and the table pass all agree."""
  legacy = [
      _record("codex:t1:0", RecordKind.NATIVE, sessions=("sess-a",), output=5),
      _record("codex:t1:1", RecordKind.NATIVE, sessions=("sess-a",), output=7),
  ]
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", legacy)
    ledger.model_rows()  # the memo and the one-time backfill
    replacements = [
        _record("codex-total:t1:5:0:2", RecordKind.NATIVE, sessions=("sess-a",), output=9),
        _record("codex-total:t1:5:0:3", RecordKind.NATIVE, sessions=("sess-a",), output=6),
    ]
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", replacements, supersede_prefix="codex:t1:")
    paths = _read_path(monkeypatch, ledger)
    rows, starts = ledger.model_rows_with_native_starts()
    served = ledger.model_rows()
    agg_rows = _rows_from_accs(ledger._agg_accs())
    table_rows = _table_pass_rows(ledger)
  assert paths == ["full"]
  assert served is rows
  assert rows == agg_rows == table_rows
  assert [(row.model, row.calls, row.output) for row in rows] == [("model-a", 2, 15)]
  assert starts == {SOURCE: "2026-01-10"}


def test_ts_conflict_keeps_the_earlier_non_empty_value(tmp_path):
  """On a record_id conflict the stored ts keeps the earlier non-empty value: a later
  stamp changes nothing (and is no rewrite), an earlier stamp wins, and an empty stamp
  never displaces a stored one."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), ts=TS_A, output=5)
  later = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), ts=TS_B, output=5)
  earlier = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), ts="2026-01-09T07:00:00+00:00", output=5)
  blank = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), ts="", output=5)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    epoch = ledger._rewrite_epoch()
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", [later])
    stored = ledger._conn.execute("SELECT ts FROM usage WHERE record_id = 'rec-1'").fetchone()[0]
    assert stored == TS_A
    assert ledger._rewrite_epoch() == epoch
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a3", [earlier])
    stored = ledger._conn.execute("SELECT ts FROM usage WHERE record_id = 'rec-1'").fetchone()[0]
    assert stored == "2026-01-09T07:00:00+00:00"
    assert ledger._rewrite_epoch() == epoch + 1
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a4", [blank])
    stored = ledger._conn.execute("SELECT ts FROM usage WHERE record_id = 'rec-1'").fetchone()[0]
    assert stored == "2026-01-09T07:00:00+00:00"
    assert ledger._rewrite_epoch() == epoch + 1
    assert ledger.model_rows() == _table_pass_rows(ledger)


def test_supersede_prefix_matching_nothing_bumps_nothing(tmp_path):
  """A prefix that matches no row deletes nothing and leaves the rewrite epoch alone."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", output=7)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    epoch = ledger._rewrite_epoch()
    ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", [second], supersede_prefix="nomatch:")
    assert ledger._rewrite_epoch() == epoch
    stored = [row["record_id"] for row in ledger._conn.execute("SELECT record_id FROM usage ORDER BY record_id")]
  assert stored == ["rec-1", "rec-2"]


# --- the one-time schema-2 upgrade ---------------------------------------------------------

# The pre-change ledger: usage and usage_agg carry four token sums, the triggers sum four
# token columns, and ledger_meta has no schema stamp -- the shape the upgrade meets on open.
_OLD_DDL = """
CREATE TABLE usage (
  record_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  source TEXT NOT NULL,
  model TEXT NOT NULL,
  account TEXT NOT NULL,
  host TEXT NOT NULL,
  ts TEXT NOT NULL,
  in_fresh INTEGER NOT NULL,
  cache_write INTEGER NOT NULL,
  cache_read INTEGER NOT NULL,
  output INTEGER NOT NULL,
  origin TEXT NOT NULL,
  captured_at TEXT NOT NULL
);
CREATE INDEX usage_group_cover ON usage(kind, source, model, account, ts, in_fresh, cache_write, cache_read, output);
CREATE TABLE usage_agg (
  source TEXT NOT NULL,
  model TEXT NOT NULL,
  account TEXT NOT NULL,
  kind TEXT NOT NULL,
  day TEXT NOT NULL,
  calls INTEGER NOT NULL,
  in_fresh INTEGER NOT NULL,
  cache_write INTEGER NOT NULL,
  cache_read INTEGER NOT NULL,
  output INTEGER NOT NULL,
  PRIMARY KEY (source, model, account, kind, day)
);
CREATE TABLE ledger_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE native_sessions (session TEXT PRIMARY KEY, source TEXT NOT NULL);
CREATE TABLE fallback_sessions (record_id TEXT NOT NULL, session TEXT NOT NULL, PRIMARY KEY (record_id, session));
CREATE TRIGGER usage_agg_after_insert AFTER INSERT ON usage
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, output)
  SELECT new.source, new.model, new.account, new.kind, SUBSTR(new.ts, 1, 10), 1,
         new.in_fresh, new.cache_write, new.cache_read, new.output
  WHERE NOT (new.kind = 'fallback' AND EXISTS (
    SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
    WHERE fs.record_id = new.record_id))
    AND NOT EXISTS (
    SELECT 1 FROM usage_agg a WHERE a.source = new.source AND a.model = new.model
      AND a.account = new.account AND a.kind = new.kind AND a.day = SUBSTR(new.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    output = output + excluded.output;
END;

CREATE TRIGGER usage_agg_after_update AFTER UPDATE ON usage
WHEN old.kind <> new.kind OR old.source <> new.source OR old.model <> new.model
  OR old.account <> new.account OR old.ts <> new.ts OR old.in_fresh <> new.in_fresh
  OR old.cache_write <> new.cache_write OR old.cache_read <> new.cache_read
  OR old.output <> new.output
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, output)
  SELECT old.source, old.model, old.account, old.kind, SUBSTR(old.ts, 1, 10), -1,
         -old.in_fresh, -old.cache_write, -old.cache_read, -old.output
  WHERE NOT (old.kind = 'fallback' AND EXISTS (
    SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
    WHERE fs.record_id = old.record_id))
    AND EXISTS (
    SELECT 1 FROM usage_agg a WHERE a.source = old.source AND a.model = old.model
      AND a.account = old.account AND a.kind = old.kind AND a.day = SUBSTR(old.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    output = output + excluded.output;
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, output)
  SELECT new.source, new.model, new.account, new.kind, SUBSTR(new.ts, 1, 10), 1,
         new.in_fresh, new.cache_write, new.cache_read, new.output
  WHERE NOT (new.kind = 'fallback' AND EXISTS (
    SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
    WHERE fs.record_id = new.record_id))
    AND NOT EXISTS (
    SELECT 1 FROM usage_agg a WHERE a.source = new.source AND a.model = new.model
      AND a.account = new.account AND a.kind = new.kind AND a.day = SUBSTR(new.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    output = output + excluded.output;
END;

CREATE TRIGGER usage_agg_after_delete AFTER DELETE ON usage
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, output)
  SELECT old.source, old.model, old.account, old.kind, SUBSTR(old.ts, 1, 10), -1,
         -old.in_fresh, -old.cache_write, -old.cache_read, -old.output
  WHERE NOT (old.kind = 'fallback' AND EXISTS (
    SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
    WHERE fs.record_id = old.record_id))
    AND EXISTS (
    SELECT 1 FROM usage_agg a WHERE a.source = old.source AND a.model = old.model
      AND a.account = old.account AND a.kind = old.kind AND a.day = SUBSTR(old.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    output = output + excluded.output;
END;

CREATE TRIGGER usage_agg_after_native_session AFTER INSERT ON native_sessions
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, output)
  SELECT u.source, u.model, u.account, u.kind, SUBSTR(u.ts, 1, 10), -1,
         -u.in_fresh, -u.cache_write, -u.cache_read, -u.output
  FROM usage u
  WHERE u.kind = 'fallback'
    AND EXISTS (SELECT 1 FROM usage_agg a WHERE a.source = u.source AND a.model = u.model
                AND a.account = u.account AND a.kind = u.kind AND a.day = SUBSTR(u.ts, 1, 10))
    AND EXISTS (SELECT 1 FROM fallback_sessions fs WHERE fs.record_id = u.record_id AND fs.session = new.session)
    AND NOT EXISTS (
      SELECT 1 FROM fallback_sessions fs2 JOIN native_sessions ns2 ON ns2.session = fs2.session
      WHERE fs2.record_id = u.record_id AND fs2.session <> new.session)
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    output = output + excluded.output;
END;

CREATE TRIGGER usage_agg_after_fallback_session AFTER INSERT ON fallback_sessions
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, output)
  SELECT u.source, u.model, u.account, u.kind, SUBSTR(u.ts, 1, 10), -1,
         -u.in_fresh, -u.cache_write, -u.cache_read, -u.output
  FROM usage u
  WHERE u.record_id = new.record_id
    AND u.kind = 'fallback'
    AND EXISTS (SELECT 1 FROM usage_agg a WHERE a.source = u.source AND a.model = u.model
                AND a.account = u.account AND a.kind = u.kind AND a.day = SUBSTR(u.ts, 1, 10))
    AND EXISTS (
      SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
      WHERE fs.record_id = u.record_id)
    AND NOT EXISTS (
      SELECT 1 FROM fallback_sessions fs2 WHERE fs2.record_id = u.record_id
      AND fs2.session <> new.session)
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    output = output + excluded.output;
END;
"""


def _old_ledger(path: Path, rows: list[tuple]) -> None:
  """Build a pre-change ledger: old four-sum DDL and triggers, the aggregate backfilled,
  and no schema stamp -- the shape the one-time upgrade meets on open."""
  raw = sqlite3.connect(path)
  try:
    raw.executescript(_OLD_DDL)
    raw.executemany(
        "INSERT INTO usage (record_id, kind, source, model, account, host, ts,"
        " in_fresh, cache_write, cache_read, output, origin, captured_at)"
        " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)", rows)
    raw.execute(
        "INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, output)"
        " SELECT source, model, account, kind, SUBSTR(ts, 1, 10), COUNT(*),"
        " SUM(in_fresh), SUM(cache_write), SUM(cache_read), SUM(output)"
        " FROM usage GROUP BY source, model, account, kind, SUBSTR(ts, 1, 10)"
        " ON CONFLICT(source, model, account, kind, day) DO UPDATE SET"
        " calls = excluded.calls, in_fresh = excluded.in_fresh, cache_write = excluded.cache_write,"
        " cache_read = excluded.cache_read, output = excluded.output")
    raw.execute("INSERT INTO ledger_meta (key, value) VALUES ('agg_backfilled', '1')")
    raw.commit()
  finally:
    raw.close()


def _upgrade_fixture_rows(root: Path, here: Path) -> list[tuple]:
  """Four pre-change records: gone- and present-source master: records, a gone-source
  native thread: record, and a gone-source codex: record."""
  gone = root / "gone"
  return [
      (
          "master:gone", "native", SOURCE, "model-m", "acct-a", HOST, TS_A, 100, 10, 20, 5, str(gone / "master.jsonl"),
          TS_A),
      ("master:here", "native", SOURCE, "model-m", "acct-a", HOST, TS_B, 200, 0, 0, 6, str(here), TS_B),
      (
          "thread:gone", "native", SOURCE, "model-t", "acct-a", HOST, TS_A, 300, 0, 0, 7, str(gone / "thread.jsonl"),
          TS_A),
      ("codex:gone", "native", SOURCE, "model-c", "acct-a", HOST, TS_B, 400, 0, 0, 8, str(gone / "codex.jsonl"), TS_B),
  ]


def _snapshot(conn) -> tuple[list[tuple], list[tuple], list[tuple]]:
  """Every usage, aggregate and meta row as plain tuples, in a stable order."""
  return (
      [tuple(row) for row in conn.execute("SELECT * FROM usage ORDER BY record_id")],
      [tuple(row) for row in conn.execute("SELECT * FROM usage_agg ORDER BY source, model, account, kind, day")],
      [tuple(row) for row in conn.execute("SELECT * FROM ledger_meta ORDER BY key")],
  )


def test_in_unsplit_shows_on_every_read_path(tmp_path, monkeypatch):
  """A record carrying in_unsplit shows the column and its total through the table pass,
  the memo fold, and the aggregate read -- the three layers the page read serves from."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), in_unsplit=7)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), model="model-b", ts=TS_B, in_unsplit=7)
  third = _record("rec-3", RecordKind.NATIVE, sessions=("sess-c",), model="model-c", in_unsplit=7)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    assert [(row.in_unsplit, row.total) for row in _table_pass_rows(ledger)] == [(7, 27)]
    ledger.model_rows()  # first read runs the backfill and stores the memo
    paths = _read_path(monkeypatch, ledger)
    ledger.record_file(HOST, "/logs/b.jsonl", "sig-b", [second])
    folded, _ = ledger.model_rows_with_native_starts()
    assert paths == ["fold"]
    assert [(row.in_unsplit, row.total) for row in folded if row.model == "model-b"] == [(7, 27)]
    with UsageLedger(path) as foreign:  # another process's shape: its own tracked inserts
      foreign.record_file(HOST, "/logs/c.jsonl", "sig-c", [third])
    served, _ = ledger.model_rows_with_native_starts()
    assert served == _table_pass_rows(ledger)
  assert [(row.model, row.in_unsplit, row.total) for row in served] == [
      ("model-a", 7, 27), ("model-b", 7, 27), ("model-c", 7, 27)
  ]


def test_upgrade_moves_only_the_gone_source_clc_records(tmp_path):
  """The one-time upgrade moves a gone-source master:/native thread: record's input into
  in_unsplit and leaves present-file and codex: rows alone; every total is unchanged, the
  aggregate read still equals the table pass, and the ledger ends stamped schema '2'."""
  path = tmp_path / "ledger.sqlite3"
  here = tmp_path / "here-master.jsonl"
  here.write_text("{}\n")
  rows = _upgrade_fixture_rows(tmp_path, here)
  _old_ledger(path, rows)
  with UsageLedger(path) as ledger:
    stored = {row["record_id"]: row for row in ledger._conn.execute("SELECT * FROM usage")}
    assert stored["master:gone"]["in_fresh"] == 0 and stored["master:gone"]["in_unsplit"] == 100
    assert stored["thread:gone"]["in_fresh"] == 0 and stored["thread:gone"]["in_unsplit"] == 300
    assert stored["master:here"]["in_fresh"] == 200 and stored["master:here"]["in_unsplit"] == 0
    assert stored["codex:gone"]["in_fresh"] == 400 and stored["codex:gone"]["in_unsplit"] == 0
    for record_id, row in stored.items():
      old = next(r for r in rows if r[0] == record_id)
      total = row["in_fresh"] + row["cache_write"] + row["cache_read"] + row["in_unsplit"] + row["output"]
      assert total == old[7] + old[8] + old[9] + old[10]  # the pre-upgrade in_fresh + cache sums + output
    served = ledger.model_rows()
    assert served == _table_pass_rows(ledger)
    assert {row.model: row.in_unsplit for row in served} == {"model-m": 100, "model-t": 300, "model-c": 0}
    meta = {row["key"]: row["value"] for row in ledger._conn.execute("SELECT * FROM ledger_meta")}
    assert meta["schema"] == "2"
    assert meta["rewrite_epoch"] == "1"


def test_upgrade_reprices_a_shared_group_day(tmp_path):
  """A moved record can share its (group, day) aggregate row with another counted record:
  the trigger's re-add lands on that shared row, and the upgrade's wholesale replacement
  re-prices the backfilled aggregate from the table, so the served rows still equal the
  table pass."""
  path = tmp_path / "ledger.sqlite3"
  here = tmp_path / "here-master.jsonl"
  here.write_text("{}\n")
  gone = tmp_path / "gone" / "master.jsonl"
  rows = [
      ("master:gone", "native", SOURCE, "model-m", "acct-a", HOST, TS_A, 100, 10, 20, 5, str(gone), TS_A),
      ("master:here", "native", SOURCE, "model-m", "acct-a", HOST, TS_A, 200, 0, 0, 6, str(here), TS_A),
  ]
  _old_ledger(path, rows)
  with UsageLedger(path) as ledger:
    stored = {row["record_id"]: row for row in ledger._conn.execute("SELECT * FROM usage")}
    assert stored["master:gone"]["in_fresh"] == 0 and stored["master:gone"]["in_unsplit"] == 100
    assert stored["master:here"]["in_fresh"] == 200 and stored["master:here"]["in_unsplit"] == 0
    served = ledger.model_rows()
    assert served == _table_pass_rows(ledger)
  assert [(row.calls, row.in_fresh, row.in_unsplit, row.total) for row in served] == [(2, 200, 100, 341)]


def test_reopening_the_upgraded_ledger_changes_no_row(tmp_path):
  """The upgrade is one-time: a ledger already stamped schema '2' opens without touching
  a usage row, an aggregate row, or the meta stamps."""
  path = tmp_path / "ledger.sqlite3"
  here = tmp_path / "here-master.jsonl"
  here.write_text("{}\n")
  _old_ledger(path, _upgrade_fixture_rows(tmp_path, here))
  with UsageLedger(path) as ledger:
    ledger.model_rows()
    before = _snapshot(ledger._conn)
  with UsageLedger(path) as ledger:
    after = _snapshot(ledger._conn)
  assert before == after


def test_in_unsplit_only_rewrite_keeps_the_aggregate_on_the_table(tmp_path):
  """A rewrite that changes only in_unsplit reaches the trigger-maintained aggregate: the
  served rows equal the table pass and carry the new split."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), in_unsplit=0)
  rewritten = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), in_unsplit=7)
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a", [first])
    ledger.model_rows()  # the backfill; later reads serve the aggregate
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", [rewritten])
    rows = ledger.model_rows()
    assert rows == _table_pass_rows(ledger)
  assert [(row.in_unsplit, row.total) for row in rows] == [(7, 27)]


# The pre-fix release's add-arm trigger bodies (d845d13a): the add skipped any (group, day)
# row that already existed, so every later insert into a served group-day was dropped and a
# rewrite's re-add never landed. Only these two bodies differ from this module's; the three
# subtract-only bodies are unchanged.
_PRE_FIX_INSERT_TRIGGER = """
CREATE TRIGGER usage_agg_after_insert AFTER INSERT ON usage
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT new.source, new.model, new.account, new.kind, SUBSTR(new.ts, 1, 10), 1,
         new.in_fresh, new.cache_write, new.cache_read, new.in_unsplit, new.output
  WHERE NOT (new.kind = 'fallback' AND EXISTS (
      SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
      WHERE fs.record_id = new.record_id))
    AND NOT EXISTS (
      SELECT 1 FROM usage_agg a WHERE a.source = new.source AND a.model = new.model
      AND a.account = new.account AND a.kind = new.kind AND a.day = SUBSTR(new.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;"""

_PRE_FIX_UPDATE_TRIGGER = """
CREATE TRIGGER usage_agg_after_update AFTER UPDATE ON usage
WHEN old.kind <> new.kind OR old.source <> new.source OR old.model <> new.model
  OR old.account <> new.account OR old.ts <> new.ts OR old.in_fresh <> new.in_fresh
  OR old.cache_write <> new.cache_write OR old.cache_read <> new.cache_read
  OR old.in_unsplit <> new.in_unsplit OR old.output <> new.output
BEGIN
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT old.source, old.model, old.account, old.kind, SUBSTR(old.ts, 1, 10), -1,
         -old.in_fresh, -old.cache_write, -old.cache_read, -old.in_unsplit, -old.output
  WHERE NOT (old.kind = 'fallback' AND EXISTS (
      SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
      WHERE fs.record_id = old.record_id))
    AND EXISTS (
      SELECT 1 FROM usage_agg a WHERE a.source = old.source AND a.model = old.model
      AND a.account = old.account AND a.kind = old.kind AND a.day = SUBSTR(old.ts, 1, 10));
  INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write, cache_read, in_unsplit, output)
  SELECT new.source, new.model, new.account, new.kind, SUBSTR(new.ts, 1, 10), 1,
         new.in_fresh, new.cache_write, new.cache_read, new.in_unsplit, new.output
  WHERE NOT (new.kind = 'fallback' AND EXISTS (
      SELECT 1 FROM fallback_sessions fs JOIN native_sessions ns ON ns.session = fs.session
      WHERE fs.record_id = new.record_id))
    AND NOT EXISTS (
      SELECT 1 FROM usage_agg a WHERE a.source = new.source AND a.model = new.model
      AND a.account = new.account AND a.kind = new.kind AND a.day = SUBSTR(new.ts, 1, 10))
  ON CONFLICT(source, model, account, kind, day) DO UPDATE SET
    calls = calls + excluded.calls, in_fresh = in_fresh + excluded.in_fresh,
    cache_write = cache_write + excluded.cache_write, cache_read = cache_read + excluded.cache_read,
    in_unsplit = in_unsplit + excluded.in_unsplit, output = output + excluded.output;
END;"""


def _trigger_sqls(conn) -> dict[str, str]:
  """The stored sql of every trigger in the ledger, for reopen-stability asserts."""
  return {
      row["name"]: row["sql"]
      for row in conn.execute("SELECT name, sql FROM sqlite_master WHERE type = 'trigger' ORDER BY name")
  }


def _write_raw_usage(raw: sqlite3.Connection, rec: UsageRecord) -> None:
  """One record through raw SQL, the way a release's record_file lands it: sessions
  first, then the usage row."""
  for session in rec.sessions:
    raw.execute("INSERT INTO native_sessions (session, source) VALUES (?, ?)", (session, rec.source))
  raw.execute(
      "INSERT INTO usage (record_id, kind, source, model, account, host, ts,"
      " in_fresh, cache_write, cache_read, in_unsplit, output, origin, captured_at)"
      " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, '/x', ?)", (
          rec.record_id, rec.kind.value, rec.source, rec.model, rec.account, HOST, rec.ts, rec.in_fresh,
          rec.cache_write, rec.cache_read, rec.in_unsplit, rec.output, rec.ts))


def _pre_fix_ledger(path: Path, served: list[UsageRecord], late: list[UsageRecord]) -> None:
  """Build a ledger as the pre-fix release left it: this schema, the add-arm-guarded
  trigger bodies, *served* records written and the aggregate backfilled over them, then
  *late* records written under the same bodies -- whose adds a served group-day drops.
  Both stamps are set, so the fixed module's open takes the trigger-refresh path."""
  raw = sqlite3.connect(path)
  try:
    raw.executescript(_SCHEMA)
    raw.executescript(_PRE_FIX_INSERT_TRIGGER + _PRE_FIX_UPDATE_TRIGGER)
    for rec in served + late:
      _write_raw_usage(raw, rec)
    raw.execute(
        "INSERT INTO usage_agg (source, model, account, kind, day, calls, in_fresh, cache_write,"
        " cache_read, in_unsplit, output)"
        " SELECT source, model, account, kind, SUBSTR(ts, 1, 10), COUNT(*),"
        " SUM(in_fresh), SUM(cache_write), SUM(cache_read), SUM(in_unsplit), SUM(output)"
        " FROM usage WHERE record_id IN"
        f" ({','.join('?' * len(served))})"
        " GROUP BY source, model, account, kind, SUBSTR(ts, 1, 10)"
        " ON CONFLICT(source, model, account, kind, day) DO UPDATE SET"
        " calls = excluded.calls, in_fresh = excluded.in_fresh, cache_write = excluded.cache_write,"
        " cache_read = excluded.cache_read, in_unsplit = excluded.in_unsplit, output = excluded.output",
        [rec.record_id for rec in served])
    raw.executemany("INSERT INTO ledger_meta (key, value) VALUES (?, ?)", [("agg_backfilled", "1"), ("schema", "2")])
    raw.commit()
  finally:
    raw.close()


def test_open_of_a_current_ledger_does_not_queue_behind_a_live_writer(tmp_path):
  """The open's trigger match check reads before it locks: against a held write
  transaction the open completes at the read's own speed instead of waiting out the
  writer's busy ceiling — every opener (CLI, page, collector) pays the open."""
  path = tmp_path / "ledger.sqlite3"
  with UsageLedger(path) as ledger:
    ledger.record_file(
        HOST, "/logs/a.jsonl", "sig-a", [_record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)])
  raw = sqlite3.connect(path, timeout=5)
  try:
    raw.execute("CREATE TABLE open_probe (x)")  # uncommitted write transaction: the write lock held
    raw.execute("INSERT INTO open_probe VALUES (1)")
    started = time.monotonic()
    with UsageLedger(path):
      pass
    assert time.monotonic() - started < 15  # the open's own read, not the 30 s writer wait
  finally:
    raw.rollback()
    raw.close()


def test_open_swaps_pre_fix_trigger_bodies_and_reprices_the_aggregate(tmp_path):
  """A ledger the pre-fix release left -- its add arm dropped every write onto an existing
  (group, day) row -- opens under the fixed module with fresh bodies and a wholesale
  re-price: the aggregate equals the table pass, a same-group-day insert keeps it equal,
  and a reopen whose bodies now match runs nothing."""
  first = _record("rec-1", RecordKind.NATIVE, sessions=("sess-a",), output=5)
  second = _record("rec-2", RecordKind.NATIVE, sessions=("sess-b",), output=6)
  path = tmp_path / "ledger.sqlite3"
  _pre_fix_ledger(path, [first], [second])  # the backfill ran before second was written
  raw = sqlite3.connect(path)
  try:
    assert raw.execute("SELECT calls FROM usage_agg").fetchall() == [(1,)]  # second's add was dropped
  finally:
    raw.close()
  third = _record("rec-3", RecordKind.NATIVE, sessions=("sess-c",), output=7)
  with UsageLedger(path) as ledger:
    served = _agg_rows(ledger)
    assert served == _table_pass_rows(ledger)
    assert [(row.calls, row.output) for row in served] == [(2, 11)]
    ledger.record_file(HOST, "/logs/a.jsonl", "sig-a2", [third])
    served = _agg_rows(ledger)
    assert served == _table_pass_rows(ledger)
  assert [(row.calls, row.output) for row in served] == [(3, 18)]
  with UsageLedger(path) as ledger:
    ledger.model_rows()
    before, triggers = _snapshot(ledger._conn), _trigger_sqls(ledger._conn)
  with UsageLedger(path) as ledger:
    after, triggers_again = _snapshot(ledger._conn), _trigger_sqls(ledger._conn)
  assert before == after
  assert triggers == triggers_again
