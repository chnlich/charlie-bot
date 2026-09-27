"""Tests for the cold-session storage sweep (src/core/storage_cool.py).

The suite pins mechanisms, not literals: the transport rule deletes by path-then-name
(uploads keep their stdout.log), rotation suffixes die only inside managed dirs, the
cold rule reads only existing metadata fields, a dry run leaves every byte and every
database page untouched, and one failing file or statement never stops the run.
"""

import json
import os
import sqlite3
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest
from conftest import backend_option

from src.core import storage_cool
from src.core.config import CharlieBotConfig
from src.core.runs import RAW_LOG_NAME
from src.core.storage_cool import (
    claude_project_dir_name,
    is_cold_session,
    run_cool_sweep,
)

NOW = datetime(2026, 9, 4, 12, 0, 0, tzinfo=UTC)
OLD = (NOW - timedelta(days=30)).isoformat()
RECENT = (NOW - timedelta(days=1)).isoformat()
CLAUDE_HOME = "claude-home"
CODEX_HOME = ".codex"

SID_COLD = "11111111-2222-4333-8444-555555555555"
SID_LIVE = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
SID_DEAD = "99999999-8888-4777-8666-555555555555"
TID = "f0e1d2c3-a4b5-4c6d-8e7f-0a1b2c3d4e5f"
CC_OPENCOLD = "ses_coldbackend0000000000000000000000"
CODEX_COLD = "0f0e1d2c-3b4a-4c5d-8e9f-0a1b2c3d4e5f"


def build_cfg(tmp_path: Path) -> CharlieBotConfig:
  """Config isolated under tmp_path: sessions, worktrees, claude and codex trees all inside it.

  Codex runs from the default home, so the codex option carries no directory of its own."""
  options = [backend_option(id="opus", label="Opus", type="cc-claude", model="m")]
  options.append(backend_option(id="codex-test", label="Codex", type="codex", model="m"))
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      paths={"worktree_dir": str(tmp_path / "worktrees")},
      backends={"options": options},
  )


@pytest.fixture
def cool_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> CharlieBotConfig:
  """Isolated stores: HOME under tmp (default claude/codex trees absent), the claude
  config dir and opencode db both under tmp."""
  monkeypatch.setenv("HOME", str(tmp_path))
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(tmp_path / CLAUDE_HOME))
  monkeypatch.setattr(storage_cool, "DEFAULT_OPENCODE_DB", tmp_path / "opencode.db")
  return build_cfg(tmp_path)


def write_session_meta(cfg: CharlieBotConfig, sid: str, meta: dict) -> Path:
  session_dir = cfg.sessions_dir / sid
  session_dir.mkdir(parents=True, exist_ok=True)
  path = session_dir / "metadata.json"
  path.write_text(json.dumps(meta), encoding="utf-8")
  return path


def cold_meta(**extra: object) -> dict:
  return {"status": "archived", "updated_at": OLD, **extra}


def live_meta(**extra: object) -> dict:
  return {"status": "active", "updated_at": RECENT, **extra}


def thread_data_dir(cfg: CharlieBotConfig, sid: str) -> Path:
  data_dir = cfg.sessions_dir / sid / "threads" / TID / "data"
  data_dir.mkdir(parents=True, exist_ok=True)
  return data_dir


def master_run_dir(cfg: CharlieBotConfig, sid: str, started_at: str = "2026-08-01T00:00:00+00:00") -> Path:
  run_dir = cfg.sessions_dir / sid / "data" / "master_runs" / started_at
  run_dir.mkdir(parents=True, exist_ok=True)
  return run_dir


def claude_projects_root(tmp_path: Path) -> Path:
  root = tmp_path / CLAUDE_HOME / "projects"
  root.mkdir(parents=True, exist_ok=True)
  return root


def claude_dir(tmp_path: Path, name: str) -> Path:
  path = claude_projects_root(tmp_path) / name
  path.mkdir(parents=True, exist_ok=True)
  (path / "transcript.jsonl").write_bytes(b"claude-transcript")
  return path


def encoded_session_dir(cfg: CharlieBotConfig, sid: str) -> str:
  return claude_project_dir_name(cfg.sessions_dir / sid)


def age_file(path: Path, age: timedelta) -> None:
  stamp = (NOW - age).timestamp()
  os.utime(path, (stamp, stamp))


def tree_bytes_snapshot(root: Path) -> dict[str, bytes]:
  """Every file's bytes under *root*, keyed by relative path; missing root means empty."""
  if not root.exists():
    return {}
  return {str(path.relative_to(root)): path.read_bytes() for path in sorted(root.rglob("*")) if path.is_file()}


# ---------------------------------------------------------------------------
# Cold rule
# ---------------------------------------------------------------------------


def test_cold_rule_requires_archived_status_and_idle_age() -> None:
  assert is_cold_session({"status": "archived", "updated_at": OLD}, now=NOW, min_idle_days=14)
  assert not is_cold_session({"status": "archived", "updated_at": RECENT}, now=NOW, min_idle_days=14)
  assert not is_cold_session({"status": "active", "updated_at": OLD}, now=NOW, min_idle_days=14)
  # Missing or unreadable metadata fields never qualify.
  assert not is_cold_session({"status": "archived"}, now=NOW, min_idle_days=14)
  assert not is_cold_session({"status": "archived", "updated_at": "not-a-date"}, now=NOW, min_idle_days=14)
  assert not is_cold_session({}, now=NOW, min_idle_days=14)


def test_metadataless_session_dir_never_qualifies(cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  orphan_dir = cfg.sessions_dir / SID_DEAD
  (orphan_dir / "threads" / TID / "data").mkdir(parents=True)
  (orphan_dir / "threads" / TID / "data" / "stdout.log").write_bytes(b"x")

  result = run_cool_sweep(cfg=cfg, now=NOW)

  assert (orphan_dir / "threads" / TID / "data" / "stdout.log").exists()
  assert result.category("raw-transport").count == 0


# ---------------------------------------------------------------------------
# Transport files: scoped by relative path, allowlist of names

# ---------------------------------------------------------------------------
# Claude Code transcript directories
# ---------------------------------------------------------------------------


def test_claude_dirs_delete_for_cold_sessions_and_keep_live_ones(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta())
  write_session_meta(cfg, SID_LIVE, live_meta())
  cold_dir = claude_dir(tmp_path, encoded_session_dir(cfg, SID_COLD))
  cold_thread_dir = claude_dir(tmp_path, f"{encoded_session_dir(cfg, SID_COLD)}-threads-{TID}")
  live_dir = claude_dir(tmp_path, encoded_session_dir(cfg, SID_LIVE))

  result = run_cool_sweep(cfg=cfg, now=NOW)

  assert not cold_dir.exists()
  assert not cold_thread_dir.exists()
  assert live_dir.exists()
  claude_result = result.category("claude-transcripts")
  assert claude_result.count == 2
  assert claude_result.bytes == 2 * len(b"claude-transcript")


def test_claude_user_cwd_dirs_never_touched(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  user_dirs = [
      claude_dir(tmp_path, "-home-dev"),
      claude_dir(tmp_path, "-home-dev-workspace-charlie-bot"),
      claude_dir(tmp_path, "-tmp-cb-e2e-home-sessions"),
  ]

  run_cool_sweep(cfg=cfg, now=NOW)

  for path in user_dirs:
    assert path.exists()


# ---------------------------------------------------------------------------
# Codex rollout files
# ---------------------------------------------------------------------------


def codex_sessions_tree(tmp_path: Path) -> Path:
  tree = tmp_path / CODEX_HOME / "sessions" / "2026" / "09" / "04"
  tree.mkdir(parents=True, exist_ok=True)
  return tree


def write_rollout(tree: Path, name: str, *, mtime: timedelta | None = None) -> Path:
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


def make_opencode_db(path: Path, aggregates: dict[str, dict]) -> None:
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
    tmp_path: Path, cool_env: CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> None:
  """VACUUM is opt-in now: even a sweep that frees pages never compacts on its own."""
  cfg = cool_env
  db = tmp_path / "opencode.db"
  make_opencode_db(db, {CC_OPENCOLD: {"events": [b"event-bytes"]}})
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))
  calls: list[bool] = []

  def pretend_vacuum(connection: sqlite3.Connection, db_path: Path, *, force: bool) -> None:
    del connection, db_path
    calls.append(force)

  monkeypatch.setattr(storage_cool, "_vacuum_opencode_db", pretend_vacuum)

  run_cool_sweep(cfg=cfg, now=NOW)
  run_cool_sweep(cfg=cfg, now=NOW, force=True)  # --force without --vacuum has no effect

  assert calls == []


# ---------------------------------------------------------------------------
# Idempotence and dry run
# ---------------------------------------------------------------------------


def _seed_every_category(tmp_path: Path, cfg: CharlieBotConfig) -> None:
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))
  (thread_data_dir(cfg, SID_COLD) / "stdout.log").write_bytes(b"transport")
  (master_run_dir(cfg, SID_COLD) / "agent.raw.ndjson").write_bytes(b"raw")
  claude_dir(tmp_path, encoded_session_dir(cfg, SID_COLD))
  write_rollout(codex_sessions_tree(tmp_path), f"rollout-2026-08-01T00-00-00-{CODEX_COLD}.jsonl")
  make_opencode_db(tmp_path / "opencode.db", {CC_OPENCOLD: {"events": [b"event-bytes"], "messages": 1}})


def test_dry_run_leaves_every_byte_untouched_and_matches_real_run(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  _seed_every_category(tmp_path, cfg)
  db = tmp_path / "opencode.db"
  db_bytes = db.read_bytes()
  sessions_before = tree_bytes_snapshot(cfg.sessions_dir)
  claude_before = tree_bytes_snapshot(tmp_path / CLAUDE_HOME)
  codex_before = tree_bytes_snapshot(tmp_path / CODEX_HOME)

  dry = run_cool_sweep(cfg=cfg, now=NOW, dry_run=True)

  assert tree_bytes_snapshot(cfg.sessions_dir) == sessions_before
  assert tree_bytes_snapshot(tmp_path / CLAUDE_HOME) == claude_before
  assert tree_bytes_snapshot(tmp_path / CODEX_HOME) == codex_before
  assert db.read_bytes() == db_bytes

  real = run_cool_sweep(cfg=cfg, now=NOW)

  assert dry.categories == real.categories
  assert dry.total_bytes == real.total_bytes


# ---------------------------------------------------------------------------
# Failure isolation

# ---------------------------------------------------------------------------
# Scoped run (--session)

# ---------------------------------------------------------------------------
# CLI and scheduler wiring

# ---------------------------------------------------------------------------
# Migrated run references: retention-protected evidence
# ---------------------------------------------------------------------------


def test_run_reference_to_outside_path_is_ignored_not_created(cool_env: CharlieBotConfig) -> None:
  """A run ref pointing outside the sessions tree cannot divert the sweep."""
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta(schema_version=2, profile="manager"))
  transport = master_run_dir(cfg, SID_COLD) / RAW_LOG_NAME
  transport.write_bytes(b"x")
  age_file(transport, timedelta(days=30))
  run_meta = {
      "id": "run-outside",
      "session_id": SID_COLD,
      "kind": "work",
      "raw_log_ref": "/nonexistent/outside/agent.raw.ndjson",
  }
  runs_dir = cfg.sessions_dir / SID_COLD / "data" / "runs" / "run-outside"
  runs_dir.mkdir(parents=True)
  (runs_dir / "metadata.json").write_text(json.dumps(run_meta), encoding="utf-8")

  run_cool_sweep(cfg=cfg, now=NOW)
  assert not transport.exists()
