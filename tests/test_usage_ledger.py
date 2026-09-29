"""Tests for the SQLite usage ledger (src/core/usage_ledger.py).

Every id, path, host and name here is synthetic and each ledger lives under tmp_path,
so no test reads the real charliebot home or any captured file. Each assertion checks
a named mechanism (dedup, exclusion, order independence, upsert stability) rather
than a hard-coded total.
"""

from __future__ import annotations

import sqlite3
from types import SimpleNamespace

import pytest

from src.core.usage_ledger import (
    _MODEL_ROWS_SQL,
    RecordKind,
    UsageLedger,
    UsageRecord,
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
    starts = ledger.native_start()
  assert starts == {SOURCE: "2026-01-11"}


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
