#!/usr/bin/env python3
"""Compare Python's and Rust's cold-storage sweep over one synthetic home.

Each side does one real run at the same instant: no dry run, no session scope, no vacuum. The synthetic home is built
once under a temporary root and copied for each side. Both copies sit at the same absolute path, because a Claude
transcript directory's name encodes that path. A run must delete exactly the fixture's expected files. The comparison
covers the remaining file trees (paths and bytes), the usage ledger's tables and the summary line. A control run appends
a line to one remaining file in the Rust copy after its run, and the script must report exactly that difference. A
second home, whose session metadata is a JSON list, must make both sweeps refuse it with their own error and delete
nothing.

usage: cool_storage_differential.py <cool-storage-sweep binary>
"""

from __future__ import annotations

import datetime
import json
import os
import pathlib
import shutil
import sqlite3
import subprocess
import sys
import tempfile

NOW = datetime.datetime(2026, 10, 19, 12, 0, 0, tzinfo=datetime.UTC)
NOW_TEXT = NOW.isoformat()
OLD = (NOW - datetime.timedelta(days=30)).isoformat()
RECENT = (NOW - datetime.timedelta(days=1)).isoformat()
REPO_ROOT = pathlib.Path(__file__).resolve().parents[5]
PACKAGES = ("src.backends.claude_code", "src.backends.codex", "src.backends.opencode")
LEDGER_DIR = "charlie/usage"
# Each table's columns and row order. `usage.captured_at` is the wall clock of the insert, so two runs differ in
# it and it is left out. `ledger_meta`'s capture timestamps are masked in `ledger_tables`.
LEDGER_TABLES = {
    "usage":
        (
            "record_id, kind, source, model, account, host, ts, in_fresh, cache_write, cache_read, in_unsplit, output, "
            "origin", "record_id"),
    "native_sessions": ("*", "session, source"),
    "fallback_sessions": ("*", "record_id, session"),
    "captured_files": ("*", "host, path"),
    "usage_agg": ("*", "source, model, account, kind, day"),
    "ledger_meta": ("*", "key"),
}

TID = "f0e1d2c3-a4b5-4c6d-8e7f-0a1b2c3d4e5f"
SID_COLD = "11111111-2222-4333-8444-555555555555"
SID_COLD_SHARED = "22222222-3333-4444-8555-666666666666"
SID_COLD_OC = "33333333-4444-4555-8666-777777777777"
SID_WARM = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
SID_UNARCHIVED = "66666666-7777-4888-8999-aaaaaaaaaaaa"
SID_NOMETA = "99999999-8888-4777-8666-555555555555"
SID_GONE = "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff"
SID_GONE_FRESH = "cccccccc-dddd-4eee-8fff-000000000000"
CODEX_COLD = "0f0e1d2c-3b4a-4c5d-8e9f-0a1b2c3d4e5f"
CODEX_SHARED = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d"
CODEX_WARM = "2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d6e"
CODEX_ORPHAN = "3c4d5e6f-7a8b-4c9d-8e0f-2a3b4c5d6e7f"
CODEX_FRESH = "4d5e6f7a-8b9c-4d0e-9f1a-3b4c5d6e7f80"
OPENCODE_COLD = "ses_cold0000000000000000000000000000"
SID_NUMERIC = "dddddddd-eeee-4fff-8000-111111111111"
SID_UNREADABLE = "eeeeeeee-ffff-4000-8111-222222222222"
SID_NAIVE = "ffffffff-0000-4111-8222-333333333333"
SID_OFFSET = "00000000-1111-4222-8333-444444444444"
SID_RECENT_OFFSET = "11111111-2222-4333-8444-555555555556"
SID_LINKED = "12121212-3434-4565-8787-898989898989"
SID_REFUSED = "13131313-3434-4565-8787-898989898989"
# The error each sweep raises for a session whose metadata is a JSON list: the cold rule calls `.get` on it.
PYTHON_REFUSAL = "'list' object has no attribute 'get'"
RUST_REFUSAL = f"session {SID_REFUSED} metadata is not a JSON object"
OPENCODE_ORPHAN = "ses_orphan000000000000000000000000"

CLAUDE_HOME = "home/.claude/projects"
CLAUDE_CONFIG = "claude-config/projects"
CODEX_DAY = "home/.codex/sessions/2026/09/04"
MASTER_OLD = "2026-08-01T00:00:00+00:00"
MASTER_SHARED = "2026-08-03T00:00:00+00:00"
MASTER_KEPT = "2026-08-02T00:00:00+00:00"
USAGE_CLAUDE = {"input_tokens": 13, "cache_creation_input_tokens": 3, "cache_read_input_tokens": 2, "output_tokens": 6}
USAGE_CODEX = {"input_tokens": 100, "cached_input_tokens": 20, "output_tokens": 8}
USAGE_MASTER = {"input_tokens": 20, "cached_tokens": 4, "output_tokens": 7}


def _lines(*values: dict) -> bytes:
  return b"".join((json.dumps(value) + "\n").encode() for value in values)


def _encoded(path: pathlib.Path) -> str:
  """Claude Code's transcript directory name for a process cwd, as the claude sweep derives it."""
  return str(path).replace("/", "-").replace(".", "-").replace("_", "-")


def _transcript(message_id: str) -> bytes:
  return _lines(
      {
          "type": "assistant",
          "timestamp": "2026-08-01T10:00:00+00:00",
          "message": {
              "id": message_id,
              "model": "vendor/claude-model",
              "usage": USAGE_CLAUDE
          },
      })


def _rollout(thread_id: str) -> bytes:
  return _lines(
      {
          "type": "session_meta",
          "payload": {
              "session_id": thread_id
          }
      },
      {
          "type": "turn_context",
          "payload": {
              "model": "vendor/gpt-rollout"
          }
      },
      {
          "type": "event_msg",
          "timestamp": "2026-08-01T10:00:00+00:00",
          "payload":
              {
                  "type": "token_count",
                  "info": {
                      "last_token_usage": USAGE_CODEX,
                      "total_token_usage": USAGE_CODEX
                  }
              },
      },
  )


def _master(model: str) -> bytes:
  return _lines({"type": "context", "model": model}, {"type": "result", "usage": USAGE_MASTER})


def _write_opencode_store(path: pathlib.Path) -> None:
  path.parent.mkdir(parents=True)
  connection = sqlite3.connect(path)
  try:
    connection.executescript(
        """
        CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
        CREATE TABLE event (id TEXT PRIMARY KEY, aggregate_id TEXT NOT NULL, seq INTEGER NOT NULL,
          type TEXT NOT NULL, data TEXT NOT NULL);
        CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
          seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
        """)
    stamp = int((NOW - datetime.timedelta(days=30)).timestamp() * 1000)
    for index, session_id in enumerate((OPENCODE_COLD, OPENCODE_ORPHAN)):
      blob = {
          "model": {
              "id": "vendor/opencode-model",
              "providerID": "opencode-account"
          },
          "tokens": {
              "input": 30,
              "output": 5,
              "cache": {
                  "read": 2,
                  "write": 1
              }
          },
          "time": {
              "created": stamp
          },
      }
      connection.execute(
          "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)"
          " VALUES (?, ?, 'assistant', 0, ?, ?, ?)", (f"msg-{index}", session_id, stamp, stamp, json.dumps(blob)))
    connection.commit()
  finally:
    connection.close()


def build_fixture(root: pathlib.Path) -> set[str]:
  """Writes the synthetic home under *root* and returns the files the sweep must delete, relative to *root*."""
  deleted: set[str] = set()

  def write(rel: str, data: bytes, *, age_days: float | None = None, gone: bool = False) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    if age_days is not None:
      stamp = (NOW - datetime.timedelta(days=age_days)).timestamp()
      os.utime(path, (stamp, stamp))
    if gone:
      deleted.add(rel)

  def session(sid: str, meta: dict) -> str:
    write(f"charlie/sessions/{sid}/metadata.json", json.dumps(meta).encode())
    return f"charlie/sessions/{sid}"

  def transport(session_dir: str, master: str, thread: str | None, gone: bool) -> None:
    """The reserved transport files of one master run and one thread, plus files the rule must keep."""
    run = f"{session_dir}/data/master_runs/{master}"
    for name in ("agent.raw.ndjson", "agent.stderr.log", "agent.raw.cursor", "agent.raw.ndjson.1"):
      write(f"{run}/{name}", _master("vendor/native-model") if name == "agent.raw.ndjson" else b"x", gone=gone)
    write(f"{run}/events.jsonl", b"", gone=False)
    write(f"{run}/stdout.log.x", b"not a rotation", gone=False)
    if thread is not None:
      data = f"{session_dir}/threads/{thread}/data"
      for name in ("stdout.log", "stderr.log", "agent.raw.ndjson.2"):
        write(f"{data}/{name}", b"x", gone=gone)
      write(f"{data}/events.jsonl", b"", gone=False)
      write(f"{data}/stdout.log.x", b"not a rotation", gone=False)

  def claude(base: str, session_dir: str | None, name: str | None, *, age_days: float | None, gone: bool) -> None:
    project = name if name is not None else _encoded(root / session_dir)
    write(f"{base}/{project}/transcript.jsonl", _transcript(f"msg-{project[-12:]}"), age_days=age_days, gone=gone)

  cold = session(SID_COLD, {"status": "archived", "updated_at": OLD, "cc_session_id": CODEX_COLD})
  cold_shared = session(SID_COLD_SHARED, {"status": "archived", "updated_at": OLD, "cc_session_id": CODEX_SHARED})
  cold_oc = session(SID_COLD_OC, {"status": "archived", "updated_at": OLD, "cc_session_id": OPENCODE_COLD})
  warm = session(SID_WARM, {"status": "archived", "updated_at": RECENT, "cc_session_id": CODEX_SHARED})
  unarchived = session(SID_UNARCHIVED, {"status": "active", "updated_at": OLD, "cc_session_id": CODEX_WARM})
  nometa = f"charlie/sessions/{SID_NOMETA}"

  # Cold: transport, and a run record that references one master log, which the sweep keeps while its unreferenced
  # sibling goes. The master logs hold usage that the capture reads before the sweep deletes them.
  transport(cold, MASTER_OLD, TID, gone=True)
  write(f"{cold}/data/master_runs/{MASTER_KEPT}/agent.raw.ndjson", _master("vendor/native-model"))
  write(f"{cold}/data/master_runs/{MASTER_KEPT}/agent.stderr.log", b"unreferenced sibling", gone=True)
  write(
      f"{cold}/data/runs/run-migrated/metadata.json",
      json.dumps(
          {
              "id": "run-migrated",
              "session_id": SID_COLD,
              "kind": "work",
              "backend": "codex-main",
              "started_at": MASTER_KEPT,
              "raw_log_ref": f"{root}/{cold}/data/master_runs/{MASTER_KEPT}/agent.raw.ndjson",
          }).encode())
  write(
      f"{cold}/data/runs/run-migrated/agent.raw.ndjson",
      _lines({
          "type": "turn.completed",
          "usage": {
              "input_tokens": 60,
              "cached_input_tokens": 20,
              "output_tokens": 8
          }
      }))
  write(f"{cold}/data/agent.raw.ndjson", b"outside the managed directories")
  write(f"{cold}/uploads/stdout.log", b"a user's upload keeps its name")
  write(f"{cold}/threads/{TID}/data/notes.txt", b"kept", gone=False)
  write(f"{cold}/threads/{TID}/agent.raw.ndjson", b"outside the data directory", gone=False)

  # Cold, sharing a rollout with a warm session: the shared rollout stays.
  transport(cold_shared, MASTER_SHARED, TID, gone=True)
  # Cold, referencing an opencode session: the sweep never deletes its store rows.
  transport(cold_oc, MASTER_OLD, None, gone=True)
  # Warm and unarchived: nothing they hold goes.
  transport(warm, MASTER_OLD, TID, gone=False)
  write(f"{warm}/uploads/agent.raw.ndjson.1", b"upload", gone=False)
  transport(unarchived, MASTER_OLD, TID, gone=False)
  write(f"{nometa}/threads/{TID}/data/stdout.log", b"no metadata", gone=False)

  # Claude Code transcript directories: cold sessions go, live ones stay, orphans go once idle.
  claude(CLAUDE_HOME, cold, None, age_days=None, gone=True)
  claude(CLAUDE_HOME, cold, f"{_encoded(root / cold)}-threads-{TID}", age_days=None, gone=True)
  claude(CLAUDE_CONFIG, cold_shared, None, age_days=None, gone=True)
  claude(CLAUDE_CONFIG, cold_oc, None, age_days=None, gone=True)
  claude(CLAUDE_HOME, warm, None, age_days=None, gone=False)
  claude(CLAUDE_HOME, unarchived, None, age_days=None, gone=False)
  claude(CLAUDE_HOME, nometa, None, age_days=None, gone=False)
  claude(CLAUDE_HOME, f"charlie/sessions/{SID_GONE}", None, age_days=30, gone=True)
  claude(CLAUDE_HOME, f"charlie/sessions/{SID_GONE_FRESH}", None, age_days=1, gone=False)
  claude(CLAUDE_HOME, None, "-home-dev-workspace-charlie-bot", age_days=30, gone=False)

  # Codex rollouts: referenced and cold goes; shared, warm and unarchived references stay; an orphan goes once idle.
  for thread, age, gone in (
      (CODEX_COLD, 30, True),
      (CODEX_SHARED, 30, False),
      (CODEX_WARM, 1, False),
      (CODEX_ORPHAN, 30, True),
      (CODEX_FRESH, 1, False),
  ):
    write(f"{CODEX_DAY}/rollout-2026-08-01T00-00-00-{thread}.jsonl", _rollout(thread), age_days=age, gone=gone)

  # Cold, with a Unicode-digit rotation suffix: `str.isdigit` counts it, so it goes. A vulgar fraction is not a digit.
  run_dir = f"{cold}/data/master_runs/{MASTER_OLD}"
  write(f"{run_dir}/agent.raw.ndjson.\u0663", b"rotation in Arabic-Indic digits", gone=True)
  write(f"{run_dir}/agent.raw.ndjson.\u2153", b"a vulgar fraction", gone=False)

  # Cold, with a numeric cc_session_id: its str() text is the reference, so the session still parses.
  numeric = session(SID_NUMERIC, {"status": "archived", "updated_at": OLD, "cc_session_id": 12345})
  transport(numeric, MASTER_OLD, None, gone=True)

  # Unreadable metadata: a directory where metadata.json belongs. The session is skipped, so its files stay.
  unreadable = f"charlie/sessions/{SID_UNREADABLE}"
  (root / unreadable / "metadata.json").mkdir(parents=True)
  transport(unreadable, MASTER_OLD, None, gone=False)

  # A naive updated_at is UTC, and an offset one is its instant: both are cold; the recent offset one is not.
  naive = session(SID_NAIVE, {"status": "archived", "updated_at": "2026-08-01T00:00:00"})
  transport(naive, MASTER_OLD, None, gone=True)
  offset = session(SID_OFFSET, {"status": "archived", "updated_at": "2026-08-01T09:00:00+09:00"})
  transport(offset, MASTER_OLD, None, gone=True)
  recent_offset = session(SID_RECENT_OFFSET, {"status": "archived", "updated_at": "2026-10-18T09:00:00+09:00"})
  transport(recent_offset, MASTER_OLD, None, gone=False)

  # A master_runs directory that is a symlink is not managed: the files behind the link stay.
  linked = session(SID_LINKED, {"status": "archived", "updated_at": OLD})
  write(f"charlie/linked-runs/{MASTER_OLD}/agent.raw.ndjson", b"behind a link", gone=False)
  (root / linked / "data").mkdir(parents=True)
  os.symlink(os.path.relpath(root / "charlie/linked-runs", root / linked / "data"), root / linked / "data/master_runs")

  write(
      "charlie/config.yaml", (
          f"paths:\n  worktree_dir: '{root}/worktrees'\n"
          "backends:\n  options:\n"
          "    - id: codex-main\n      label: Codex\n      type: codex\n"
          "      model: vendor/codex-model\n"
          "    - id: claude-main\n      label: Claude\n      type: cc-claude\n"
          "      model: vendor/claude-model\n").encode())
  _write_opencode_store(root / "charlie/opencode/data/opencode/opencode.db")
  return deleted


def build_refusal(root: pathlib.Path) -> None:
  """A home whose session metadata is a JSON list: the cold rule fails on it, before any deletion."""
  refused = root / "charlie/sessions" / SID_REFUSED
  refused.mkdir(parents=True)
  (refused / "metadata.json").write_bytes(b"[1, 2]")
  cold = root / "charlie/sessions" / SID_COLD
  (cold / "data/master_runs" / MASTER_OLD).mkdir(parents=True)
  (cold / "data/master_runs" / MASTER_OLD / "agent.raw.ndjson").write_bytes(b"kept: the run refuses")
  (cold / "metadata.json").write_bytes(json.dumps({"status": "archived", "updated_at": OLD}).encode())
  (root / "charlie/config.yaml").write_bytes(b"paths:\n  worktree_dir: '" + str(root / "worktrees").encode() + b"'\n")


def _assert_inside(root: pathlib.Path, path: pathlib.Path) -> None:
  if not pathlib.Path(os.path.realpath(path)).is_relative_to(os.path.realpath(root)):
    raise SystemExit(f"{path} resolves outside the synthetic tree {root}; nothing was swept")


def python_sweep(root: pathlib.Path) -> str:
  """Python's real run, in a child process whose environment points every store into *root*."""
  import importlib

  for package in PACKAGES:
    importlib.import_module(package).register()
  from src.backends.claude_code import login_dirs
  from src.backends.claude_code import usage_sweep as claude_sweep
  from src.backends.codex import codex_usage
  from src.features.usage import storage_cool
  from src.infra import config, home

  cfg = config.get_config()
  stores = [
      cfg.charliebot_home,
      home.opencode_db(),
      login_dirs.claude_config_dir(),
      login_dirs.default_claude_dir(),
      codex_usage.default_codex_home(),
      *claude_sweep.claude_projects_roots(cfg),
  ]
  for store in stores:
    _assert_inside(root, store)
  return storage_cool.format_sweep_line(storage_cool.run_cool_sweep(cfg=cfg, now=NOW))


def _side_environment(root: pathlib.Path) -> dict[str, str]:
  return {
      "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
      "HOME": str(root / "home"),
      "CHARLIEBOT_HOME": str(root / "charlie"),
      "CLAUDE_CONFIG_DIR": str(root / "claude-config"),
      "PYTHONDONTWRITEBYTECODE": "1",
  }


def _summary_line(output: str) -> str:
  summary = output.strip().splitlines()[-1]
  if not summary.startswith("total "):
    raise SystemExit(f"no summary line in the sweep output:\n{output}")
  return summary


def _python_child(root: pathlib.Path) -> subprocess.CompletedProcess[str]:
  return subprocess.run(
      [sys.executable, str(pathlib.Path(__file__).resolve()), "python-sweep",
       str(root)],
      cwd=REPO_ROOT,
      env=_side_environment(root),
      capture_output=True,
      text=True,
      check=False,
  )


def _rust_child(binary: pathlib.Path, root: pathlib.Path) -> subprocess.CompletedProcess[str]:
  return subprocess.run(
      [str(binary), str(root), NOW_TEXT],
      env=_side_environment(root),
      capture_output=True,
      text=True,
      check=False,
  )


def run_python(root: pathlib.Path) -> str:
  completed = _python_child(root)
  if completed.returncode != 0:
    raise SystemExit(f"the Python sweep failed:\n{completed.stderr}")
  return _summary_line(completed.stdout)


def run_rust(binary: pathlib.Path, root: pathlib.Path) -> str:
  completed = _rust_child(binary, root)
  if completed.returncode != 0:
    raise SystemExit(f"the Rust sweep failed:\n{completed.stderr}")
  return _summary_line(completed.stdout)


def file_tree(root: pathlib.Path) -> dict[str, bytes]:
  """Every file under *root* outside the ledger directory, keyed by its relative path."""
  files = {}
  for path in sorted(root.rglob("*")):
    rel = path.relative_to(root).as_posix()
    if path.is_file() and not rel.startswith(LEDGER_DIR + "/"):
      files[rel] = path.read_bytes()
  return files


def dir_tree(root: pathlib.Path) -> set[str]:
  return {
      path.relative_to(root).as_posix()
      for path in root.rglob("*")
      if path.is_dir() and not path.relative_to(root).as_posix().startswith(LEDGER_DIR)
  }


def ledger_tables(root: pathlib.Path) -> dict[str, list[tuple]]:
  """The usage ledger's tables in a stable order; the capture timestamp is omitted, as it differs per run."""
  path = root / LEDGER_DIR / "ledger.sqlite3"
  if not path.exists():
    raise SystemExit(f"the sweep wrote no usage ledger at {path}")
  connection = sqlite3.connect(path)
  try:
    tables = {
        table: [tuple(row) for row in connection.execute(f"SELECT {columns} FROM {table} ORDER BY {order}")]
        for table, (columns, order) in LEDGER_TABLES.items()
    }
  finally:
    connection.close()
  tables["ledger_meta"] = [
      (key, "<timestamp omitted>" if key.startswith("last_capture_at:") else value)
      for key, value in tables["ledger_meta"]
  ]
  return tables


def collect(root: pathlib.Path, summary: str) -> dict:
  return {
      "files": file_tree(root),
      "dirs": dir_tree(root),
      "ledger": ledger_tables(root),
      "summary": summary,
  }


def differences(left: dict, right: dict) -> list[str]:
  found = []
  if left["summary"] != right["summary"]:
    found.append(f"summary line: {left['summary']!r} != {right['summary']!r}")
  for rel in sorted(set(left["files"]) | set(right["files"])):
    if rel not in right["files"]:
      found.append(f"file only in Python's tree: {rel}")
    elif rel not in left["files"]:
      found.append(f"file only in Rust's tree: {rel}")
    elif left["files"][rel] != right["files"][rel]:
      found.append(f"bytes differ: {rel}")
  for rel in sorted(left["dirs"] ^ right["dirs"]):
    found.append(f"directory present on one side only: {rel}")
  for table in LEDGER_TABLES:
    left_rows, right_rows = left["ledger"][table], right["ledger"][table]
    if left_rows == right_rows:
      continue
    first = next(
        (index for index, (a, b) in enumerate(zip(left_rows, right_rows)) if a != b),
        min(len(left_rows), len(right_rows)))
    python_row, rust_row = left_rows[first:first + 1], right_rows[first:first + 1]
    found.append(f"ledger table {table} differs at row {first}: python={python_row!r} rust={rust_row!r}")
  return found


def _fresh_copy(snapshot: pathlib.Path, root: pathlib.Path) -> None:
  if root.exists():
    shutil.rmtree(root)
  shutil.copytree(snapshot, root, symlinks=True)


def check_refusal(binary: pathlib.Path, scratch: pathlib.Path) -> None:
  """Both sweeps refuse a home whose session metadata is a JSON list, with their own error, and delete nothing."""
  run = scratch / "refusal"
  snapshot = scratch / "refusal-snapshot"
  run.mkdir()
  build_refusal(run)
  before = file_tree(run)
  shutil.copytree(run, snapshot, symlinks=True)

  _fresh_copy(snapshot, run)
  python = _python_child(run)
  _assert_refused("Python", python, PYTHON_REFUSAL, run, before)

  _fresh_copy(snapshot, run)
  rust = _rust_child(binary, run)
  _assert_refused("Rust", rust, RUST_REFUSAL, run, before)

  print("refusal: a home whose session metadata is a JSON list; both sweeps refuse it and delete nothing")
  print(f"  python: exit {python.returncode}; {python.stderr.strip().splitlines()[-1]}")
  print(f"  rust:   exit {rust.returncode}; {rust.stderr.strip().splitlines()[-1]}")


def _assert_refused(
    side: str, completed: subprocess.CompletedProcess[str], reason: str, run: pathlib.Path,
    before: dict[str, bytes]) -> None:
  # The reason must appear in stderr: a sandbox-guard failure also exits non-zero, and must not pass as a refusal.
  if completed.returncode == 0 or reason not in completed.stderr:
    raise SystemExit(
        f"the {side} sweep must refuse with {reason!r}; it exited {completed.returncode}:\n{completed.stderr}")
  _check_deleted(before, file_tree(run), set(), side)


def main(argv: list[str]) -> int:
  if len(argv) != 2:
    raise SystemExit("usage: cool_storage_differential.py <cool-storage-sweep binary>")
  binary = pathlib.Path(argv[1]).resolve()
  if not binary.is_file():
    raise SystemExit(f"the Rust sweep binary is missing: {binary}")

  with tempfile.TemporaryDirectory(prefix="cool-storage-differential-") as scratch:
    run = pathlib.Path(scratch) / "run"
    snapshot = pathlib.Path(scratch) / "snapshot"
    run.mkdir()
    expected = build_fixture(run)
    before = file_tree(run)
    shutil.copytree(run, snapshot, symlinks=True)

    _fresh_copy(snapshot, run)
    python_summary = run_python(run)
    python_state = collect(run, python_summary)
    _check_deleted(before, python_state["files"], expected, "Python")

    _fresh_copy(snapshot, run)
    rust_summary = run_rust(binary, run)
    rust_state = collect(run, rust_summary)
    _check_deleted(before, rust_state["files"], expected, "Rust")

    found = differences(python_state, rust_state)
    print(f"differential: real run at {NOW_TEXT}; {len(expected)} files expected deleted on both sides")
    print(f"  python: {python_summary}")
    print(f"  rust:   {rust_summary}")
    for line in found:
      print(f"  difference: {line}")
    print(f"  differences: {len(found)}")

    planted = pathlib.Path("charlie/sessions") / SID_WARM / "data/master_runs" / MASTER_OLD / "agent.raw.ndjson"
    _fresh_copy(snapshot, run)
    rust_summary = run_rust(binary, run)
    with (run / planted).open("ab") as handle:
      handle.write(b"planted difference\n")
    control = differences(python_state, collect(run, rust_summary))
    print(f"control: a line appended to {planted.as_posix()} in the Rust copy after its run")
    for line in control:
      print(f"  difference: {line}")
    print(f"  differences: {len(control)}")
    check_refusal(binary, pathlib.Path(scratch))

  if found or len(control) != 1 or not control[0].startswith(f"bytes differ: {planted.as_posix()}"):
    print("FAILED: the differential must report zero differences, and the control must report its planted one")
    return 1
  print("OK: zero differences on the real run, and the planted difference was the one reported")
  return 0


def _check_deleted(before: dict[str, bytes], after: dict[str, bytes], expected: set[str], side: str) -> None:
  removed = set(before) - set(after)
  if removed != expected:
    raise SystemExit(f"the {side} sweep deleted {sorted(removed)}; the rule expects {sorted(expected)}")


if __name__ == "__main__":
  if len(sys.argv) == 3 and sys.argv[1] == "python-sweep":
    print(python_sweep(pathlib.Path(sys.argv[2])))
    raise SystemExit(0)
  raise SystemExit(main(sys.argv))
