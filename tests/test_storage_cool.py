"""Tests for the cold-session storage sweep (src/features/storage/storage_cool.py).

The suite pins mechanisms, not literals: the transport rule deletes by path-then-name
(uploads keep their stdout.log), rotation suffixes die only inside managed dirs, the
cold rule reads only existing metadata fields, a dry run leaves every byte and every
database page untouched, and one failing file or statement never stops the run.
"""

import asyncio
import datetime
import json
import os
import pathlib
import sqlite3

import conftest
import pytest

from src.features.storage import storage_cool
from src.infra import config
from src.runtime import runs

NOW = datetime.datetime(2026, 9, 4, 12, 0, 0, tzinfo=datetime.UTC)
OLD = (NOW - datetime.timedelta(days=30)).isoformat()
RECENT = (NOW - datetime.timedelta(days=1)).isoformat()
CLAUDE_HOME = "claude-home"
CODEX_HOME = ".codex"

SID_COLD = "11111111-2222-4333-8444-555555555555"
SID_LIVE = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
SID_DEAD = "99999999-8888-4777-8666-555555555555"
TID = "f0e1d2c3-a4b5-4c6d-8e7f-0a1b2c3d4e5f"
CC_OPENCOLD = "ses_coldbackend0000000000000000000000"
CODEX_COLD = "0f0e1d2c-3b4a-4c5d-8e9f-0a1b2c3d4e5f"


def build_cfg(tmp_path: pathlib.Path) -> config.CharlieBotConfig:
  """Config isolated under tmp_path: sessions, worktrees, claude and codex trees all inside it.

  Codex runs from the default home, so the codex option carries no directory of its own."""
  options = [conftest.backend_option(id="opus", label="Opus", type="cc-claude", model="m")]
  options.append(conftest.backend_option(id="codex-test", label="Codex", type="codex", model="m"))
  return config.CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
      backends={"options": options},
  )


@pytest.fixture
def cool_env(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> config.CharlieBotConfig:
  """Isolated stores: HOME under tmp (default claude/codex trees absent), the claude
  config dir and opencode db both under tmp."""
  monkeypatch.setenv("HOME", str(tmp_path))
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / CLAUDE_HOME))
  monkeypatch.setattr(storage_cool, "DEFAULT_OPENCODE_DB", tmp_path / "opencode.db")
  return build_cfg(tmp_path)


@pytest.fixture(autouse=True)
def no_real_usage_capture(monkeypatch: pytest.MonkeyPatch) -> None:
  """No sweep reads the real home's ledger: the pre-sweep capture is a no-op unless
  a test overrides it."""
  monkeypatch.setattr(storage_cool, "_capture_usage_before_sweep", dict)


def write_session_meta(cfg: config.CharlieBotConfig, sid: str, meta: dict) -> pathlib.Path:
  session_dir = cfg.sessions_dir / sid
  session_dir.mkdir(parents=True, exist_ok=True)
  path = session_dir / "metadata.json"
  path.write_text(json.dumps(meta), encoding="utf-8")
  return path


def cold_meta(**extra: object) -> dict:
  return {"status": "archived", "updated_at": OLD, **extra}


def live_meta(**extra: object) -> dict:
  return {"status": "active", "updated_at": RECENT, **extra}


def thread_data_dir(cfg: config.CharlieBotConfig, sid: str) -> pathlib.Path:
  data_dir = cfg.sessions_dir / sid / "threads" / TID / "data"
  data_dir.mkdir(parents=True, exist_ok=True)
  return data_dir


def master_run_dir(
    cfg: config.CharlieBotConfig, sid: str, started_at: str = "2026-08-01T00:00:00+00:00") -> pathlib.Path:
  run_dir = cfg.sessions_dir / sid / "data" / "master_runs" / started_at
  run_dir.mkdir(parents=True, exist_ok=True)
  return run_dir


def claude_projects_root(tmp_path: pathlib.Path) -> pathlib.Path:
  root = tmp_path / CLAUDE_HOME / "projects"
  root.mkdir(parents=True, exist_ok=True)
  return root


def claude_dir(tmp_path: pathlib.Path, name: str) -> pathlib.Path:
  path = claude_projects_root(tmp_path) / name
  path.mkdir(parents=True, exist_ok=True)
  (path / "transcript.jsonl").write_bytes(b"claude-transcript")
  return path


def encoded_session_dir(cfg: config.CharlieBotConfig, sid: str) -> str:
  return storage_cool.claude_project_dir_name(cfg.sessions_dir / sid)


def age_file(path: pathlib.Path, age: datetime.timedelta) -> None:
  stamp = (NOW - age).timestamp()
  os.utime(path, (stamp, stamp))


def tree_bytes_snapshot(root: pathlib.Path) -> dict[str, bytes]:
  """Every file's bytes under *root*, keyed by relative path; missing root means empty."""
  if not root.exists():
    return {}
  return {str(path.relative_to(root)): path.read_bytes() for path in sorted(root.rglob("*")) if path.is_file()}


# ---------------------------------------------------------------------------
# Cold rule
# ---------------------------------------------------------------------------


def test_cold_rule_requires_archived_status_and_idle_age() -> None:
  assert storage_cool.is_cold_session({"status": "archived", "updated_at": OLD}, now=NOW, min_idle_days=14)
  assert not storage_cool.is_cold_session({"status": "archived", "updated_at": RECENT}, now=NOW, min_idle_days=14)
  assert not storage_cool.is_cold_session({"status": "active", "updated_at": OLD}, now=NOW, min_idle_days=14)
  # Missing or unreadable metadata fields never qualify.
  assert not storage_cool.is_cold_session({"status": "archived"}, now=NOW, min_idle_days=14)
  assert not storage_cool.is_cold_session({"status": "archived", "updated_at": "not-a-date"}, now=NOW, min_idle_days=14)
  assert not storage_cool.is_cold_session({}, now=NOW, min_idle_days=14)


def test_metadataless_session_dir_never_qualifies(cool_env: config.CharlieBotConfig) -> None:
  cfg = cool_env
  orphan_dir = cfg.sessions_dir / SID_DEAD
  (orphan_dir / "threads" / TID / "data").mkdir(parents=True)
  (orphan_dir / "threads" / TID / "data" / "stdout.log").write_bytes(b"x")

  result = storage_cool.run_cool_sweep(cfg=cfg, now=NOW)

  assert (orphan_dir / "threads" / TID / "data" / "stdout.log").exists()
  assert result.category("raw-transport").count == 0


# ---------------------------------------------------------------------------
# Claude Code transcript directories
# ---------------------------------------------------------------------------


def test_claude_dirs_delete_for_cold_sessions_and_keep_live_ones(
    tmp_path: pathlib.Path, cool_env: config.CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta())
  write_session_meta(cfg, SID_LIVE, live_meta())
  cold_dir = claude_dir(tmp_path, encoded_session_dir(cfg, SID_COLD))
  cold_thread_dir = claude_dir(tmp_path, f"{encoded_session_dir(cfg, SID_COLD)}-threads-{TID}")
  live_dir = claude_dir(tmp_path, encoded_session_dir(cfg, SID_LIVE))

  result = storage_cool.run_cool_sweep(cfg=cfg, now=NOW)

  assert not cold_dir.exists()
  assert not cold_thread_dir.exists()
  assert live_dir.exists()
  claude_result = result.category("claude-transcripts")
  assert claude_result.count == 2
  assert claude_result.bytes == 2 * len(b"claude-transcript")


def test_claude_user_cwd_dirs_never_touched(tmp_path: pathlib.Path, cool_env: config.CharlieBotConfig) -> None:
  cfg = cool_env
  user_dirs = [
      claude_dir(tmp_path, "-home-dev"),
      claude_dir(tmp_path, "-home-dev-workspace-charlie-bot"),
      claude_dir(tmp_path, "-tmp-cb-e2e-home-sessions"),
  ]

  storage_cool.run_cool_sweep(cfg=cfg, now=NOW)

  for path in user_dirs:
    assert path.exists()


# ---------------------------------------------------------------------------
# Codex rollout files
# ---------------------------------------------------------------------------


def codex_sessions_tree(tmp_path: pathlib.Path) -> pathlib.Path:
  tree = tmp_path / CODEX_HOME / "sessions" / "2026" / "09" / "04"
  tree.mkdir(parents=True, exist_ok=True)
  return tree


def write_rollout(tree: pathlib.Path, name: str, *, mtime: datetime.timedelta | None = None) -> pathlib.Path:
  path = tree / name
  path.write_bytes(b"rollout")
  if mtime is not None:
    age_file(path, mtime)
  return path


# ---------------------------------------------------------------------------
# opencode event store
# ---------------------------------------------------------------------------

_OPENCODE_SCHEMA = """
CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
CREATE TABLE event (
  id TEXT PRIMARY KEY,
  aggregate_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  type TEXT NOT NULL,
  data TEXT NOT NULL,
  CONSTRAINT fk_event_aggregate FOREIGN KEY (aggregate_id)
    REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE
);
CREATE INDEX event_aggregate_seq_idx ON event (aggregate_id, seq);
CREATE TABLE session (id TEXT PRIMARY KEY, time_updated INTEGER);
CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);
CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, data TEXT NOT NULL);
"""


def make_opencode_db(path: pathlib.Path, aggregates: dict[str, dict]) -> None:
  """Fixture store with opencode's shape: a sequence row per aggregate, event rows
  carrying the bytes behind an ON DELETE CASCADE foreign key, message rows for
  usage accounting.  ``event_sizes`` builds the payload with ``zeroblob`` so a
  worst-case-sized row does not cost its byte count in Python memory."""
  path.parent.mkdir(parents=True, exist_ok=True)
  connection = sqlite3.connect(path)
  try:
    connection.executescript(_OPENCODE_SCHEMA)
    for aggregate_id, spec in aggregates.items():
      connection.execute(
          "insert into event_sequence (aggregate_id, seq, owner_id) values (?, 0, NULL)", (aggregate_id,))
      connection.execute(
          "insert into session (id, time_updated) values (?, ?)",
          (aggregate_id, spec.get("updated_ms", int(NOW.timestamp() * 1000))))
      for index, payload in enumerate(spec.get("events", [])):
        connection.execute(
            "insert into event (id, aggregate_id, seq, type, data) values (?, ?, ?, 'message.updated', ?)",
            (f"{aggregate_id}-e{index}", aggregate_id, index, payload))
      for index, size in enumerate(spec.get("event_sizes", [])):
        connection.execute(
            "insert into event (id, aggregate_id, seq, type, data) values (?, ?, ?, 'message.updated', zeroblob(?))",
            (f"{aggregate_id}-z{index}", aggregate_id, index, size))
      for index in range(spec.get("messages", 0)):
        connection.execute(
            "insert into message (id, session_id, data) values (?, ?, '{}')",
            (f"{aggregate_id}-m{index}", aggregate_id))
    connection.commit()
  finally:
    connection.close()


def test_default_sweep_never_vacuums(
    tmp_path: pathlib.Path, cool_env: config.CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> None:
  """VACUUM is opt-in now: even a sweep that frees pages never compacts on its own."""
  cfg = cool_env
  db = tmp_path / "opencode.db"
  make_opencode_db(db, {CC_OPENCOLD: {"events": [b"event-bytes"]}})
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))
  calls: list[bool] = []

  def pretend_vacuum(connection: sqlite3.Connection, db_path: pathlib.Path, *, force: bool) -> None:
    del connection, db_path
    calls.append(force)

  monkeypatch.setattr(storage_cool, "_vacuum_opencode_db", pretend_vacuum)

  storage_cool.run_cool_sweep(cfg=cfg, now=NOW)
  storage_cool.run_cool_sweep(cfg=cfg, now=NOW, force=True)  # --force without --vacuum has no effect

  assert calls == []


# ---------------------------------------------------------------------------
# Idempotence and dry run
# ---------------------------------------------------------------------------


def _seed_every_category(tmp_path: pathlib.Path, cfg: config.CharlieBotConfig) -> None:
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))
  (thread_data_dir(cfg, SID_COLD) / "stdout.log").write_bytes(b"transport")
  (master_run_dir(cfg, SID_COLD) / "agent.raw.ndjson").write_bytes(b"raw")
  claude_dir(tmp_path, encoded_session_dir(cfg, SID_COLD))
  write_rollout(codex_sessions_tree(tmp_path), f"rollout-2026-08-01T00-00-00-{CODEX_COLD}.jsonl")
  make_opencode_db(tmp_path / "opencode.db", {CC_OPENCOLD: {"events": [b"event-bytes"], "messages": 1}})


def test_dry_run_leaves_every_byte_untouched_and_matches_real_run(
    tmp_path: pathlib.Path, cool_env: config.CharlieBotConfig) -> None:
  cfg = cool_env
  _seed_every_category(tmp_path, cfg)
  db = tmp_path / "opencode.db"
  db_bytes = db.read_bytes()
  sessions_before = tree_bytes_snapshot(cfg.sessions_dir)
  claude_before = tree_bytes_snapshot(tmp_path / CLAUDE_HOME)
  codex_before = tree_bytes_snapshot(tmp_path / CODEX_HOME)

  dry = storage_cool.run_cool_sweep(cfg=cfg, now=NOW, dry_run=True)

  assert tree_bytes_snapshot(cfg.sessions_dir) == sessions_before
  assert tree_bytes_snapshot(tmp_path / CLAUDE_HOME) == claude_before
  assert tree_bytes_snapshot(tmp_path / CODEX_HOME) == codex_before
  assert db.read_bytes() == db_bytes

  real = storage_cool.run_cool_sweep(cfg=cfg, now=NOW)

  assert dry.categories == real.categories
  assert dry.total_bytes == real.total_bytes


# ---------------------------------------------------------------------------
# Usage ledger capture before deletion
# ---------------------------------------------------------------------------


def test_real_sweep_aborts_without_deleting_when_capture_fails(
    tmp_path: pathlib.Path, cool_env: config.CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> None:
  """The capture guards every deletion: if it raises, the sweep deletes nothing."""
  cfg = cool_env
  _seed_every_category(tmp_path, cfg)
  sessions_before = tree_bytes_snapshot(cfg.sessions_dir)
  claude_before = tree_bytes_snapshot(tmp_path / CLAUDE_HOME)
  codex_before = tree_bytes_snapshot(tmp_path / CODEX_HOME)
  db = tmp_path / "opencode.db"
  db_bytes = db.read_bytes()

  def failing_capture() -> dict[str, int]:
    raise RuntimeError("ledger capture failed")

  monkeypatch.setattr(storage_cool, "_capture_usage_before_sweep", failing_capture)

  with pytest.raises(RuntimeError, match="ledger capture failed"):
    storage_cool.run_cool_sweep(cfg=cfg, now=NOW)

  assert tree_bytes_snapshot(cfg.sessions_dir) == sessions_before
  assert tree_bytes_snapshot(tmp_path / CLAUDE_HOME) == claude_before
  assert tree_bytes_snapshot(tmp_path / CODEX_HOME) == codex_before
  assert db.read_bytes() == db_bytes


def test_real_sweep_captures_once_before_first_deletion_dry_run_never(
    tmp_path: pathlib.Path, cool_env: config.CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> None:
  """A real sweep captures exactly once while every cold-session file is still
  there; a dry run never captures."""
  cfg = cool_env
  _seed_every_category(tmp_path, cfg)
  transport = master_run_dir(cfg, SID_COLD) / runs.RAW_LOG_NAME
  existed_at_capture: list[bool] = []

  def recording_capture() -> dict[str, int]:
    existed_at_capture.append(transport.exists())
    return {"claude": 1}

  monkeypatch.setattr(storage_cool, "_capture_usage_before_sweep", recording_capture)

  storage_cool.run_cool_sweep(cfg=cfg, now=NOW)

  assert existed_at_capture == [True]
  assert not transport.exists()

  existed_at_capture.clear()
  storage_cool.run_cool_sweep(cfg=cfg, now=NOW, dry_run=True)
  assert existed_at_capture == []


# ---------------------------------------------------------------------------
# Migrated run references: retention-protected evidence
# ---------------------------------------------------------------------------


def test_usage_ledger_handler_summarizes_and_propagates(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The scheduler's daily capture returns one line per source and never swallows
  a capture failure."""
  from src.features.usage import usage_ledger

  monkeypatch.setattr("src.features.usage.usage_ledger.default_ledger_path", lambda: tmp_path / "ledger.sqlite3")
  monkeypatch.setattr("src.features.usage.token_tally.capture_local", lambda ledger: {"claude": 3, "opencode": 7})

  summary = asyncio.run(usage_ledger.run_scheduled_usage_ledger())

  assert summary == "claude 3; opencode 7"
  assert (tmp_path / "ledger.sqlite3").exists()

  def failing(ledger: object) -> dict[str, int]:
    raise RuntimeError("ledger capture failed")

  monkeypatch.setattr("src.features.usage.token_tally.capture_local", failing)
  with pytest.raises(RuntimeError, match="ledger capture failed"):
    asyncio.run(usage_ledger.run_scheduled_usage_ledger())


def test_run_reference_to_outside_path_is_ignored_not_created(cool_env: config.CharlieBotConfig) -> None:
  """A run ref pointing outside the sessions tree cannot divert the sweep."""
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta(schema_version=2, profile="manager"))
  transport = master_run_dir(cfg, SID_COLD) / runs.RAW_LOG_NAME
  transport.write_bytes(b"x")
  age_file(transport, datetime.timedelta(days=30))
  run_meta = {
      "id": "run-outside",
      "session_id": SID_COLD,
      "kind": "work",
      "raw_log_ref": "/nonexistent/outside/agent.raw.ndjson",
  }
  runs_dir = cfg.sessions_dir / SID_COLD / "data" / "runs" / "run-outside"
  runs_dir.mkdir(parents=True)
  (runs_dir / "metadata.json").write_text(json.dumps(run_meta), encoding="utf-8")

  storage_cool.run_cool_sweep(cfg=cfg, now=NOW)
  assert not transport.exists()
