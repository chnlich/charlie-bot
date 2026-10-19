#!/usr/bin/env python3
"""Compare Python and Rust usage captures over one synthetic sessions tree."""

from __future__ import annotations

import copy
import json
import os
import pathlib
import sqlite3
import subprocess
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

HOST = "fixture-host"
NATIVE_MODEL = "vendor/native-model"
CONFIG = """backends:
  options:
    - id: charlie-code-native
      label: Native
      type: charlie-code
      model: vendor/native-model
    - id: codex-main
      label: Codex
      type: codex
      model: vendor/codex-model
    - id: claude-main
      label: Claude
      type: cc-claude
      model: vendor/claude-model
"""


def _line(value: dict) -> bytes:
  return (json.dumps(value) + "\n").encode()


def _thread(
    home: pathlib.Path,
    session: str,
    thread: str,
    backend: str | None,
    model: str | None,
    events: bytes,
) -> pathlib.Path:
  directory = home / "sessions" / session / "threads" / thread
  (directory / "data").mkdir(parents=True)
  if backend is not None:
    metadata = {"backend": backend}
    if model is not None:
      metadata["model"] = model
    (directory / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")
  path = directory / "data" / "events.jsonl"
  path.write_bytes(events)
  return path


def _master(home: pathlib.Path, session: str, started: str, events: bytes) -> pathlib.Path:
  path = home / "sessions" / session / "data" / "master_runs" / started / "agent.raw.ndjson"
  path.parent.mkdir(parents=True)
  path.write_bytes(events)
  return path


def _run(
    home: pathlib.Path,
    run_id: str,
    backend: str,
    model: str | None,
    native_session_id: str | None,
    events: bytes,
) -> pathlib.Path:
  directory = home / "sessions" / "run-session" / "data" / "runs" / run_id
  directory.mkdir(parents=True)
  metadata = {"backend": backend, "started_at": "2026-09-11T20:00:00+00:00"}
  if model is not None:
    metadata["model"] = model
  if native_session_id is not None:
    metadata["native_session_id"] = native_session_id
  (directory / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")
  path = directory / "agent.raw.ndjson"
  path.write_bytes(events)
  return path


def build_fixture(home: pathlib.Path) -> pathlib.Path:
  home.mkdir(parents=True)
  (home / "config.yaml").write_text(CONFIG, encoding="utf-8")
  live_ids = home / "codex" / "live_cli_sessions.txt"
  live_ids.parent.mkdir()
  live_ids.write_text("codex-live\nlive-run\n", encoding="utf-8")

  result = {
      "type": "result",
      "timestamp": "2026-09-11T20:00:00+00:00",
      "usage":
          {
              "input_tokens": 120,
              "cache_creation_input_tokens": 5,
              "cache_read_input_tokens": 30,
              "output_tokens": 7,
          },
  }
  _thread(
      home,
      "session-native",
      "native",
      "charlie-code-native",
      "vendor/recorded-model",
      _line(result) + _line({"session_id": "native-session"}) +
      b'{"type": "result", broken}\n{"type": "result", "usage": {"input_tokens": 999}',
  )
  _thread(home, "session-native", "native-no-id", "charlie-code-native", None, _line(result))

  codex_thread = {
      "type": "result",
      "timestamp": "2026-09-12T20:00:00+00:00",
      "usage": {
          "input_tokens": 100,
          "cache_read_input_tokens": 30,
          "output_tokens": 14
      },
  }
  _thread(
      home,
      "session-codex",
      "codex-offline",
      "codex-main",
      "vendor/gpt-offline",
      _line(codex_thread) + _line({"session_id": "codex-offline"}),
  )
  _thread(
      home,
      "session-codex",
      "codex-live",
      "codex-main",
      None,
      _line(codex_thread) + _line({"session_id": "codex-live"}),
  )
  _thread(home, "session-codex", "codex-no-id", "codex-main", None, _line(codex_thread))

  fallback_thread = {
      "type": "result",
      "timestamp": "2026-09-13T20:00:00+00:00",
      "usage":
          {
              "input_tokens": 40,
              "cache_creation_input_tokens": 8,
              "cache_read_input_tokens": 12,
              "output_tokens": 9,
          },
  }
  _thread(
      home,
      "session-claude",
      "claude-kept",
      "claude-main",
      "vendor/claude-模型",
      _line(fallback_thread) + _line({"session_id": "claude-kept"}),
  )
  _thread(home, "session-claude", "claude-no-id", "claude-main", None, _line(fallback_thread))
  _thread(
      home,
      "session-retired",
      "retired",
      "codex-retired-gpt-legacy",
      None,
      _line(codex_thread) + _line({"session_id": "codex-retired-id"}),
  )
  _thread(home, "session-unknown", "unknown", "unknown-backend", None, _line(result))
  _thread(home, "session-missing-meta", "thread", None, None, _line(result))
  _thread(home, "session-unicode", "utf", "charlie-code-native", "vendor/模型-🚀", _line(result))

  master_time = "2026-09-01T00:00:00Z"
  master = (
      _line({
          "type": "context",
          "model": NATIVE_MODEL
      }) + _line({
          "type": "result",
          "usage": {
              "input_tokens": 20,
              "cached_tokens": 4,
              "output_tokens": 7
          }
      }) + _line({
          "type": "result",
          "usage": {
              "input_tokens": 30,
              "cached_tokens": 9,
              "output_tokens": 11
          }
      }))
  _master(home, "session-master", master_time, master)
  _master(home, "session-master", "2026-09-02T00:00:00Z", _line({"type": "result", "usage": {"input_tokens": 8}}))
  _master(home, "session-master", "2026-09-03T00:00:00Z", _line({"type": "context", "model": NATIVE_MODEL}))

  native_run = (
      _line({
          "type": "result",
          "usage": {
              "input_tokens": 18,
              "cached_tokens": 4,
              "output_tokens": 7
          }
      }) + b'{"type": "result", broken}\n{"type": "result", "usage": {"input_tokens": 999}')
  _run(home, "run-native", "charlie-code-native", None, None, native_run)
  codex_run = (
      _line({
          "type": "turn.completed",
          "usage": {
              "input_tokens": 60,
              "cached_input_tokens": 20,
              "output_tokens": 8
          }
      }) +
      _line({
          "type": "turn.completed",
          "usage": {
              "input_tokens": 40,
              "cached_input_tokens": 10,
              "output_tokens": 6
          }
      }) + _line({
          "type": "thread.started",
          "thread_id": "codex-offline-run"
      }))
  _run(home, "run-codex", "codex-main", "vendor/gpt-run", None, codex_run)
  live_run = (
      _line({
          "type": "turn.completed",
          "usage": {
              "input_tokens": 8,
              "cached_input_tokens": 2,
              "output_tokens": 1
          }
      }) + _line({
          "type": "thread.started",
          "thread_id": "live-run"
      }))
  _run(home, "run-codex-live", "codex-main", None, None, live_run)
  _run(home, "run-codex-no-id", "codex-main", None, None, live_run.replace(b"live-run", b""))
  claude_run = (
      _line({"session_id": "claude-run"}) + _line(
          {
              "type": "result",
              "usage":
                  {
                      "input_tokens": 13,
                      "cache_creation_input_tokens": 3,
                      "cache_read_input_tokens": 2,
                      "output_tokens": 6,
                  },
          }))
  _run(home, "run-claude", "claude-main", None, "claude-run", claude_run)
  _run(
      home, "run-claude-no-id", "claude-main", None, None,
      _line({
          "type": "result",
          "usage": {
              "input_tokens": 13,
              "output_tokens": 6
          }
      }))
  _run(
      home, "run-unknown", "unknown-backend", None, "unknown",
      _line({
          "type": "result",
          "usage": {
              "input_tokens": 13,
              "output_tokens": 6
          }
      }))
  _run(
      home, "run-resultless", "charlie-code-native", None, None,
      _line({
          "type": "thread.started",
          "thread_id": "unused"
      }))
  return live_ids


class FixtureCodex:

  def __init__(self, live_ids: pathlib.Path):
    self.live_ids = live_ids

  def live_cli_sessions(self) -> set[str]:
    return set(self.live_ids.read_text(encoding="utf-8").splitlines())


class FixtureWithoutLiveSessions:
  pass


def python_capture(home: pathlib.Path, ledger_path: pathlib.Path, live_ids: pathlib.Path) -> dict:
  os.environ["HOME"] = str(home)
  os.environ["CHARLIEBOT_HOME"] = str(home)
  from src.features.usage import token_tally, usage_ledger
  from src.runtime.hooks import usage_source_registration, usage_sources

  options = {
      "charlie-code-native": SimpleNamespace(id="charlie-code-native", type="charlie-code", model=NATIVE_MODEL),
      "codex-main": SimpleNamespace(id="codex-main", type="codex", model="vendor/codex-model"),
      "claude-main": SimpleNamespace(id="claude-main", type="cc-claude", model="vendor/claude-model"),
  }
  sources = {
      "CLC": usage_source_registration.UsageSource("CLC", ("charlie-code-", "clc-"), True, None),
      "Codex": usage_source_registration.UsageSource("Codex", ("codex-",), False, "fixture.codex"),
      "Claude Code": usage_source_registration.UsageSource("Claude Code", ("claude-",), False, "fixture.claude"),
  }
  type_sources = {"charlie-code": "CLC", "codex": "Codex", "cc-claude": "Claude Code"}
  codex = FixtureCodex(live_ids)

  def implementation(source):
    return codex if source.name == "Codex" else FixtureWithoutLiveSessions()

  with (
      patch.object(usage_source_registration, "_sources", sources),
      patch.object(usage_source_registration, "_type_sources", type_sources),
      patch.object(token_tally, "backend_registry", return_value=options),
      patch.object(usage_sources, "implementation", side_effect=implementation),
      usage_ledger.UsageLedger(ledger_path) as ledger,
  ):
    captured = ledger.captured_sigs(HOST)
    with ledger.batch():
      thread_count = token_tally.capture_charliebot(ledger, HOST, home / "sessions", captured)
      run_count = token_tally.capture_runs(ledger, HOST, home / "sessions", captured)
      ledger.mark_capture_finished(HOST)
    rows, starts = ledger.model_rows_with_native_starts()
    return {"thread_count": thread_count, "run_count": run_count, "rows": rows_value(rows, starts)}


def rows_value(rows, starts: dict[str, str]) -> dict:
  return {
      "rows":
          [
              {
                  "source":
                      row.source,
                  "model":
                      row.model,
                  "in_fresh":
                      row.in_fresh,
                  "cache_write":
                      row.cache_write,
                  "cache_read":
                      row.cache_read,
                  "in_unsplit":
                      row.in_unsplit,
                  "output":
                      row.output,
                  "calls":
                      row.calls,
                  "total":
                      row.total,
                  "first":
                      row.first,
                  "last":
                      row.last,
                  "fallback_calls":
                      row.fallback_calls,
                  "fallback_output":
                      row.fallback_output,
                  "accounts":
                      [
                          {
                              "name": account.name,
                              "calls": account.calls,
                              "output": account.output,
                              "total": account.total
                          } for account in row.accounts
                      ],
              } for row in rows
          ],
      "native_starts": starts,
  }


def _ordered_rows(connection: sqlite3.Connection, table: str) -> list[tuple]:
  orders = {
      "usage": "record_id",
      "native_sessions": "session, source",
      "fallback_sessions": "record_id, session",
      "captured_files": "host, path",
      "usage_agg": "source, model, account, kind, day",
      "ledger_meta": "key",
  }
  return [tuple(row) for row in connection.execute(f"SELECT * FROM {table} ORDER BY {orders[table]}")]


def snapshot(home: pathlib.Path, ledger_path: pathlib.Path) -> dict[str, list[tuple]]:
  connection = sqlite3.connect(ledger_path)
  try:
    rows = {
        table: _ordered_rows(connection, table)
        for table in ("usage", "native_sessions", "fallback_sessions", "captured_files", "usage_agg", "ledger_meta")
    }
  finally:
    connection.close()
  normalized_usage = []
  for row in rows["usage"]:
    origin = pathlib.Path(row[12])
    normalized_usage.append((
        *row[:12],
        origin.relative_to(home).as_posix(),
    ))
  rows["usage"] = normalized_usage
  rows["captured_files"] = [
      (host, pathlib.Path(path).relative_to(home).as_posix(), sig) for host, path, sig in rows["captured_files"]
  ]
  rows["ledger_meta"] = [
      (key, "<timestamp omitted>" if key.startswith("last_capture_at:") else value)
      for key, value in rows["ledger_meta"]
  ]
  return rows


def differences(left: dict[str, list[tuple]], right: dict[str, list[tuple]]) -> list[str]:
  keys = {
      "usage": lambda row: row[0],
      "native_sessions": lambda row: row[0],
      "fallback_sessions": lambda row: row[:2],
      "captured_files": lambda row: row[:2],
      "usage_agg": lambda row: row[:5],
      "ledger_meta": lambda row: row[0],
  }
  changed = []
  for table, key in keys.items():
    before = {key(row): row for row in left[table]}
    after = {key(row): row for row in right[table]}
    for row_key in before.keys() | after.keys():
      if before.get(row_key) != after.get(row_key):
        changed.append(f"{table}:{row_key}")
  return changed


def run_rust_capture(
    rust_bin: pathlib.Path, home: pathlib.Path, ledger_path: pathlib.Path, live_ids: pathlib.Path) -> dict:
  environment = os.environ.copy()
  environment.update({
      "HOME": str(home),
      "CHARLIEBOT_HOME": str(home),
      "FIXTURE_LIVE_SESSIONS": str(live_ids),
  })
  result = subprocess.run(
      [str(rust_bin), "capture", str(home), str(ledger_path)],
      check=True,
      capture_output=True,
      text=True,
      env=environment,
  )
  return json.loads(result.stdout)


def run_differential(rust_bin: pathlib.Path) -> tuple[str, str]:
  with tempfile.TemporaryDirectory(prefix="usage-differential-") as temporary:
    root = pathlib.Path(temporary)
    home = root / "synthetic-home"
    live_ids = build_fixture(home)
    python_result = python_capture(home, root / "python-ledger" / "ledger.sqlite3", live_ids)
    rust_result = run_rust_capture(rust_bin, home, root / "rust-ledger" / "ledger.sqlite3", live_ids)
    if python_result["rows"] != rust_result["rows"]:
      raise AssertionError("Python and Rust model rows or native starts differ")
    python_snapshot = snapshot(home, root / "python-ledger" / "ledger.sqlite3")
    rust_snapshot = snapshot(home, root / "rust-ledger" / "ledger.sqlite3")
    delta = differences(python_snapshot, rust_snapshot)
    if delta:
      raise AssertionError(f"Python/Rust tables differ: {delta}")
    control = copy.deepcopy(rust_snapshot)
    if not control["usage_agg"]:
      raise AssertionError("fixture did not create a usage_agg row")
    changed_row = list(control["usage_agg"][0])
    changed_row[10] += 1
    control["usage_agg"][0] = tuple(changed_row)
    control_delta = differences(rust_snapshot, control)
    if len(control_delta) != 1:
      raise AssertionError(f"the planted control difference was not isolated: {control_delta}")
    return (
        f"DIFFERENTIAL: 0 differences across 6 tables; Python rows={python_result['thread_count'] + python_result['run_count']}, Rust sources={rust_result['sources']}",
        f"CONTROL: 1 difference ({control_delta[0]})",
    )


def main() -> int:
  if len(sys.argv) != 2:
    raise SystemExit("usage: differential.py <usage-fixture-binary>")
  rust_bin = pathlib.Path(sys.argv[1]).resolve()
  if not rust_bin.is_file():
    raise SystemExit(f"Rust fixture binary is missing: {rust_bin}")
  for summary in run_differential(rust_bin):
    print(summary)
  return 0


if __name__ == "__main__":
  raise SystemExit(main())
