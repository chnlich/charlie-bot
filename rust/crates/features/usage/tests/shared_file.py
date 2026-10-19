#!/usr/bin/env python3
"""Check that Python and Rust can read and repair the same usage ledger file."""

from __future__ import annotations

import pathlib
import sqlite3
import subprocess
import sys
import tempfile
import unittest

from differential import build_fixture, python_capture, run_rust_capture, snapshot


RUST_BIN = pathlib.Path(sys.argv.pop(1)).resolve()
HOST = "fixture-host"
TRIGGER_NAMES = (
    "usage_agg_after_insert",
    "usage_agg_after_update",
    "usage_agg_after_delete",
    "usage_agg_after_native_session",
    "usage_agg_after_fallback_session",
)


def trigger_and_aggregate(path: pathlib.Path) -> tuple[list[tuple[str, str]], list[tuple]]:
  connection = sqlite3.connect(path)
  try:
    triggers = connection.execute(
        "SELECT name, sql FROM sqlite_master WHERE type = 'trigger' ORDER BY name").fetchall()
    aggregate = connection.execute("SELECT * FROM usage_agg ORDER BY source, model, account, kind, day").fetchall()
    return triggers, aggregate
  finally:
    connection.close()


def python_ledger_module():
  from src.features.usage import usage_ledger

  return usage_ledger


def rust_open(path: pathlib.Path) -> None:
  subprocess.run([str(RUST_BIN), "open", str(path)], check=True, capture_output=True, text=True)


def create_python_ledger(path: pathlib.Path) -> None:
  from src.runtime.hooks import usage_sources

  ledger_module = python_ledger_module()
  record = usage_sources.UsageRecord(
      record_id="fixture-record",
      kind=usage_sources.RecordKind.NATIVE,
      source="Codex",
      model="fixture-model",
      account="fixture-account",
      ts="2026-10-18T10:00:00+00:00",
      in_fresh=11,
      cache_write=2,
      cache_read=3,
      in_unsplit=4,
      output=5,
      sessions=("fixture-session",),
  )
  with ledger_module.UsageLedger(path) as ledger:
    ledger.record_file("fixture-host", "/home/fixture/sessions/log.jsonl", "fixture-sig", [record])
    ledger.model_rows()


def create_mismatched_ledger(path: pathlib.Path) -> None:
  create_python_ledger(path)
  connection = sqlite3.connect(path)
  try:
    for name in TRIGGER_NAMES:
      connection.execute(f"DROP TRIGGER {name}")
      connection.execute(f"CREATE TRIGGER {name} AFTER INSERT ON usage BEGIN SELECT 1; END")
    connection.execute("DELETE FROM usage_agg")
    connection.execute(
        "INSERT INTO usage_agg VALUES ('wrong', 'wrong', 'wrong', 'native', '2020-01-01', 9, 9, 9, 9, 9, 9)")
    connection.commit()
  finally:
    connection.close()


class SharedUsageLedgerTests(unittest.TestCase):
  def test_python_created_ledger_opens_in_rust_without_trigger_or_aggregate_changes(self) -> None:
    with tempfile.TemporaryDirectory(prefix="usage-python-ledger-") as temporary:
      path = pathlib.Path(temporary) / "ledger.sqlite3"
      create_python_ledger(path)
      before = trigger_and_aggregate(path)
      rust_open(path)
      self.assertEqual(trigger_and_aggregate(path), before)

  def test_rust_created_ledger_opens_in_python_without_trigger_or_aggregate_changes(self) -> None:
    with tempfile.TemporaryDirectory(prefix="usage-rust-ledger-") as temporary:
      path = pathlib.Path(temporary) / "ledger.sqlite3"
      subprocess.run([str(RUST_BIN), "fill", str(path)], check=True, capture_output=True, text=True)
      before = trigger_and_aggregate(path)
      with python_ledger_module().UsageLedger(path):
        pass
      self.assertEqual(trigger_and_aggregate(path), before)

  def test_rust_capture_model_rows_and_native_starts_match_python(self) -> None:
    with tempfile.TemporaryDirectory(prefix="usage-shared-capture-") as temporary:
      root = pathlib.Path(temporary)
      home = root / "synthetic-home"
      live_ids = build_fixture(home)
      python_result = python_capture(home, root / "python" / "ledger.sqlite3", live_ids)
      rust_result = run_rust_capture(RUST_BIN, home, root / "rust" / "ledger.sqlite3", live_ids)
      self.assertEqual(python_result["rows"], rust_result["rows"])
      self.assertEqual(snapshot(home, root / "python" / "ledger.sqlite3"), snapshot(home, root / "rust" / "ledger.sqlite3"))

  def test_python_and_rust_open_swap_five_trigger_bodies_and_reprice_aggregate(self) -> None:
    with tempfile.TemporaryDirectory(prefix="usage-trigger-refresh-") as temporary:
      root = pathlib.Path(temporary)
      expected_path = root / "expected.sqlite3"
      create_python_ledger(expected_path)
      expected = trigger_and_aggregate(expected_path)

      rust_path = root / "rust.sqlite3"
      create_mismatched_ledger(rust_path)
      rust_open(rust_path)
      self.assertEqual(trigger_and_aggregate(rust_path), expected)

      python_path = root / "python.sqlite3"
      create_mismatched_ledger(python_path)
      with python_ledger_module().UsageLedger(python_path):
        pass
      self.assertEqual(trigger_and_aggregate(python_path), expected)


if __name__ == "__main__":
  if not RUST_BIN.is_file():
    raise SystemExit(f"Rust fixture binary is missing: {RUST_BIN}")
  unittest.main(argv=[sys.argv[0]], verbosity=2)
