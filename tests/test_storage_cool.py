"""Tests for the cold-session storage sweep (src/core/storage_cool.py).

The suite pins mechanisms, not literals: the transport rule deletes by path-then-name
(uploads keep their stdout.log), rotation suffixes die only inside managed dirs, the
cold rule reads only existing metadata fields, a dry run leaves every byte and every
database page untouched, and one failing file or statement never stops the run.
"""

import asyncio
import json
import os
import sqlite3
import sys
from datetime import UTC, datetime, timedelta
from pathlib import Path

import pytest

from src.cli import storage as storage_cli
from src.core import scheduler as scheduler_module
from src.core import storage_cool
from src.core.config import CharlieBotConfig
from src.core.models import BackendOption
from src.core.storage_cool import (
    claude_project_dir_name,
    codex_rollout_session_id,
    is_cold_session,
    run_cool_sweep,
)

NOW = datetime(2026, 9, 4, 12, 0, 0, tzinfo=UTC)
OLD = (NOW - timedelta(days=30)).isoformat()
RECENT = (NOW - timedelta(days=1)).isoformat()
CLAUDE_HOME = "claude-home"
CODEX_HOME = "codex-home"
WORKTREE_TRASH_NAME = ".trash"

SID_COLD = "11111111-2222-4333-8444-555555555555"
SID_LIVE = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
SID_DEAD = "99999999-8888-4777-8666-555555555555"
TID = "f0e1d2c3-a4b5-4c6d-8e7f-0a1b2c3d4e5f"
CC_OPENCOLD = "ses_coldbackend0000000000000000000000"
CC_OPENLIVE = "ses_livebackend0000000000000000000000"
CC_OPENORPHAN = "ses_orphanbackend00000000000000000000"
CODEX_COLD = "0f0e1d2c-3b4a-4c5d-8e9f-0a1b2c3d4e5f"
CODEX_LIVE = "10203040-5060-4708-90a0-b0c0d0e0f010"


def build_cfg(tmp_path: Path, *, with_codex: bool = True) -> CharlieBotConfig:
  """Config isolated under tmp_path: sessions, worktrees, claude and codex trees all inside it."""
  options = [BackendOption(id="opus", label="Opus", type="cc-claude", model="m")]
  if with_codex:
    options.append(
        BackendOption(id="codex-test", label="Codex", type="codex", model="m", codex_home=str(tmp_path / CODEX_HOME)))
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      worktree_dir=str(tmp_path / "worktrees"),
      backend_options=options,
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


def thread_data_dir(cfg: CharlieBotConfig, sid: str, tid: str = TID) -> Path:
  data_dir = cfg.sessions_dir / sid / "threads" / tid / "data"
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


def test_transport_rule_scopes_by_relative_path_not_name(cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta())
  uploads = cfg.sessions_dir / SID_COLD / "uploads"
  artifacts = cfg.sessions_dir / SID_COLD / "artifacts"
  data_root = cfg.sessions_dir / SID_COLD / "data"
  uploads.mkdir()
  artifacts.mkdir()
  data_root.mkdir()
  (uploads / "stdout.log").write_bytes(b"upload-bytes")
  (artifacts / "stdout.log").write_bytes(b"artifact-bytes")
  (data_root / "stdout.log").write_bytes(b"data-root-bytes")
  (thread_data_dir(cfg, SID_COLD) / "stdout.log").write_bytes(b"thread-bytes")
  (master_run_dir(cfg, SID_COLD) / "stdout.log").write_bytes(b"run-bytes")

  result = run_cool_sweep(cfg=cfg, now=NOW)

  # Same-named files outside the two managed directories are byte-identical.
  assert (uploads / "stdout.log").read_bytes() == b"upload-bytes"
  assert (artifacts / "stdout.log").read_bytes() == b"artifact-bytes"
  assert (data_root / "stdout.log").read_bytes() == b"data-root-bytes"
  # Only the managed directories lose their reserved names.
  assert not (thread_data_dir(cfg, SID_COLD) / "stdout.log").exists()
  assert not (master_run_dir(cfg, SID_COLD) / "stdout.log").exists()
  assert result.category("raw-transport").count == 2
  assert result.category("raw-transport").bytes == len(b"thread-bytes") + len(b"run-bytes")


def test_transport_rule_removes_rotation_suffixes_only_inside_managed_dirs(cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta())
  uploads = cfg.sessions_dir / SID_COLD / "uploads"
  uploads.mkdir()
  (uploads / "agent.raw.ndjson.1").write_bytes(b"upload")
  data_dir = thread_data_dir(cfg, SID_COLD)
  (data_dir / "agent.raw.ndjson.1").write_bytes(b"raw-1")
  (data_dir / "agent.stderr.log.2").write_bytes(b"stderr-2")
  (master_run_dir(cfg, SID_COLD) / "agent.raw.ndjson.3").write_bytes(b"raw-3")

  run_cool_sweep(cfg=cfg, now=NOW)

  assert (uploads / "agent.raw.ndjson.1").read_bytes() == b"upload"
  assert not (data_dir / "agent.raw.ndjson.1").exists()
  assert not (data_dir / "agent.stderr.log.2").exists()
  assert not (master_run_dir(cfg, SID_COLD) / "agent.raw.ndjson.3").exists()


def test_transport_rule_leaves_unrecognized_names_inside_managed_dirs(cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta())
  data_dir = thread_data_dir(cfg, SID_COLD)
  kept = {
      "events.jsonl": b"events",
      "hang_diagnostics.json": b"diag",
      "task.md": b"task",
      "recovered_from_opencode.md": b"recovered",
      # A non-digit suffix is not a rotation variant: the name rule stays an allowlist.
      "agent.raw.ndjson.old": b"old-raw",
      "stdout.log.bak": b"bak",
  }
  for name, payload in kept.items():
    (data_dir / name).write_bytes(payload)

  run_cool_sweep(cfg=cfg, now=NOW)

  for name, payload in kept.items():
    assert (data_dir / name).read_bytes() == payload


def test_transport_rule_does_not_follow_managed_directory_symlinks(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta())
  external = tmp_path / "external"
  external.mkdir()
  external_file = external / "stdout.log"
  external_file.write_bytes(b"external")
  managed_link = cfg.sessions_dir / SID_COLD / "threads" / TID / "data"
  managed_link.parent.mkdir(parents=True)
  managed_link.symlink_to(external, target_is_directory=True)

  run_cool_sweep(cfg=cfg, now=NOW)

  assert external_file.read_bytes() == b"external"


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


def test_claude_orphan_dirs_deleted_past_window_and_kept_within_it(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  dead_old = claude_dir(tmp_path, f"-tmp-somehome--charliebot-sessions-{SID_DEAD}")
  dead_recent = claude_dir(tmp_path, f"-tmp-otherhome--charliebot-sessions-{SID_DEAD}")
  age_file(dead_old / "transcript.jsonl", timedelta(days=3))
  age_file(dead_recent / "transcript.jsonl", timedelta(hours=1))

  run_cool_sweep(cfg=cfg, now=NOW)

  assert not dead_old.exists()
  assert dead_recent.exists()


def test_claude_dir_for_session_with_unreadable_metadata_is_not_an_orphan(
    tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  session_dir = cfg.sessions_dir / SID_DEAD
  session_dir.mkdir(parents=True)
  (session_dir / "metadata.json").write_text("not-json", encoding="utf-8")
  transcript_dir = claude_dir(tmp_path, encoded_session_dir(cfg, SID_DEAD))
  age_file(transcript_dir / "transcript.jsonl", timedelta(days=3))

  run_cool_sweep(cfg=cfg, now=NOW)

  assert transcript_dir.exists()


def test_claude_deleted_worktree_dirs_deleted_and_live_worktrees_kept(
    tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  worktrees = Path(cfg.worktree_dir)
  worktrees.mkdir(parents=True)
  live_worktree = worktrees / "charliebot-task-100-alive"
  live_worktree.mkdir()
  gone = claude_dir(tmp_path, claude_project_dir_name(worktrees / "charliebot-task-200-gone"))
  trash = claude_dir(tmp_path, claude_project_dir_name(worktrees / WORKTREE_TRASH_NAME / "charliebot-task-300"))
  alive = claude_dir(tmp_path, claude_project_dir_name(live_worktree))
  age_file(gone / "transcript.jsonl", timedelta(days=5))
  age_file(trash / "transcript.jsonl", timedelta(days=5))

  run_cool_sweep(cfg=cfg, now=NOW)

  assert not gone.exists()
  assert not trash.exists()
  assert alive.exists()


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


def test_claude_orphan_window_reads_newest_file_not_dir_mtime(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  """A project dir whose only file is recent survives even when the dir itself is old."""
  cfg = cool_env
  recent_dir = claude_dir(tmp_path, f"-tmp-home--charliebot-sessions-{SID_DEAD}")
  age_file(recent_dir, timedelta(days=30))
  age_file(recent_dir / "transcript.jsonl", timedelta(hours=1))

  run_cool_sweep(cfg=cfg, now=NOW)

  assert recent_dir.exists()


def test_claude_transcript_name_encodes_cwd_with_dots_and_underscores() -> None:
  assert claude_project_dir_name(Path("home/dev/.charliebot/sessions/abc")) == \
      "home-dev--charliebot-sessions-abc"


def test_encoded_session_id_reads_the_session_from_any_managed_shape() -> None:
  sid = SID_COLD
  assert storage_cool._encoded_session_id(f"-home-dev--charliebot-sessions-{sid}") == sid
  assert storage_cool._encoded_session_id(f"-home-dev--charliebot-sessions-{sid}-threads-{TID}") == sid
  assert storage_cool._encoded_session_id(f"-home-dev--charliebot-sessions-{sid}-artifacts") == sid
  assert storage_cool._encoded_session_id(f"-tmp-cb-e2e-home-sessions-{sid}") == sid
  # Not CharlieBot's shape: no sessions segment, a bare sessions dir, a truncated
  # id, or characters trailing the id.
  assert storage_cool._encoded_session_id("-home-dev") is None
  assert storage_cool._encoded_session_id("-home-dev--charliebot-sessions") is None
  assert storage_cool._encoded_session_id(f"-home-dev--charliebot-sessions-{sid}extra") is None
  assert storage_cool._encoded_session_id("-home-dev--charliebot-sessions-not-a-uuid") is None


# ---------------------------------------------------------------------------
# Codex rollout files
# ---------------------------------------------------------------------------


def test_codex_rollout_filename_parses_session_id_past_hyphenated_timestamp() -> None:
  assert codex_rollout_session_id(Path(f"rollout-2026-09-04T12-30-45-{CODEX_COLD}.jsonl")) == CODEX_COLD
  # A compact timestamp shape parses the same way: the id is the last five groups.
  assert codex_rollout_session_id(Path(f"rollout-20260904T123045-{CODEX_COLD}.jsonl")) == CODEX_COLD
  assert codex_rollout_session_id(Path("rollout-not-a-uuid.jsonl")) is None
  assert codex_rollout_session_id(Path(f"other-2026-09-04-{CODEX_COLD}.jsonl")) is None


def codex_sessions_tree(tmp_path: Path) -> Path:
  tree = tmp_path / CODEX_HOME / "sessions" / "2026" / "09" / "04"
  tree.mkdir(parents=True, exist_ok=True)
  return tree


def write_rollout(tree: Path, name: str, payload: bytes = b"rollout", *, mtime: timedelta | None = None) -> Path:
  path = tree / name
  path.write_bytes(payload)
  if mtime is not None:
    age_file(path, mtime)
  return path


def test_codex_rollouts_follow_session_cold_and_orphan_window_rules(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CODEX_COLD))
  write_session_meta(cfg, SID_LIVE, live_meta(cc_session_id=CODEX_LIVE))
  tree = codex_sessions_tree(tmp_path)
  cold_rollout = write_rollout(tree, f"rollout-2026-08-01T00-00-00-{CODEX_COLD}.jsonl")
  live_rollout = write_rollout(tree, f"rollout-2026-08-01T00-00-01-{CODEX_LIVE}.jsonl")
  orphan_old = write_rollout(tree, f"rollout-2026-08-01T00-00-02-{SID_DEAD}.jsonl", mtime=timedelta(days=3))
  orphan_recent = write_rollout(tree, f"rollout-2026-08-01T00-00-03-{SID_LIVE}.jsonl", mtime=timedelta(hours=1))

  result = run_cool_sweep(cfg=cfg, now=NOW)

  assert not cold_rollout.exists()
  assert live_rollout.exists()
  assert not orphan_old.exists()
  assert orphan_recent.exists()
  assert result.category("codex-rollouts").count == 2


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
  usage accounting."""
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
      for index in range(spec.get("messages", 0)):
        connection.execute(
            "insert into message (id, session_id, data) values (?, ?, '{}')",
            (f"{aggregate_id}-m{index}", aggregate_id))
    connection.commit()
  finally:
    connection.close()


def opencode_row_counts(db: Path) -> dict[str, int]:
  connection = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
  try:
    return {
        table: connection.execute(f"select count(*) from {table}").fetchone()[0]
        for table in ("event_sequence", "event", "message", "part")
    }
  finally:
    connection.close()


def set_opencode_updated(db: Path, aggregate_id: str, updated_ms: int) -> None:
  connection = sqlite3.connect(db)
  try:
    connection.execute("update session set time_updated = ? where id = ?", (updated_ms, aggregate_id))
    connection.commit()
  finally:
    connection.close()


def test_opencode_delete_cascades_to_exactly_one_aggregates_events(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  db = tmp_path / "opencode.db"
  payload_a, payload_b = b"a" * 100, b"b" * 100
  make_opencode_db(
      db, {
          CC_OPENCOLD: {
              "events": [payload_a, payload_a],
              "messages": 2
          },
          CC_OPENLIVE: {
              "events": [payload_b],
              "messages": 3
          },
          CC_OPENORPHAN: {
              "events": [payload_a],
              "messages": 1
          },
      })
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))
  write_session_meta(cfg, SID_LIVE, live_meta(cc_session_id=CC_OPENLIVE))
  set_opencode_updated(db, CC_OPENORPHAN, int((NOW - timedelta(days=3)).timestamp() * 1000))

  result = run_cool_sweep(cfg=cfg, now=NOW)

  connection = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
  try:
    surviving = {row[0] for row in connection.execute("select aggregate_id from event_sequence")}
    event_owners = {row[0] for row in connection.execute("select distinct aggregate_id from event")}
    message_counts = dict(connection.execute("select session_id, count(*) from message group by session_id"))
  finally:
    connection.close()
  # The cold session's and the unreferenced-but-idle aggregate lose everything;
  # the live-referenced aggregate loses nothing.
  assert surviving == {CC_OPENLIVE}
  assert event_owners == {CC_OPENLIVE}
  # Usage accounting reads message only; its rows are untouched.
  assert message_counts == {CC_OPENCOLD: 2, CC_OPENLIVE: 3, CC_OPENORPHAN: 1}
  opencode_result = result.category("opencode-events")
  assert opencode_result.count == 2
  assert opencode_result.bytes == 3 * len(payload_a)


def test_opencode_unreferenced_recent_aggregate_survives_window(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  db = tmp_path / "opencode.db"
  make_opencode_db(db, {CC_OPENORPHAN: {"events": [b"fresh"]}})
  set_opencode_updated(db, CC_OPENORPHAN, int((NOW - timedelta(hours=1)).timestamp() * 1000))

  run_cool_sweep(cfg=cfg, now=NOW)

  assert opencode_row_counts(db) == {"event_sequence": 1, "event": 1, "message": 0, "part": 0}


def test_opencode_sequence_without_events_is_reclaimed_and_messages_remain(
    tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  db = tmp_path / "opencode.db"
  make_opencode_db(db, {CC_OPENCOLD: {"events": [], "messages": 1}})
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))

  result = run_cool_sweep(cfg=cfg, now=NOW)

  assert opencode_row_counts(db) == {"event_sequence": 0, "event": 0, "message": 1, "part": 0}
  assert result.category("opencode-events").count == 1
  assert result.category("opencode-events").bytes == 0


def test_opencode_vacuum_is_retried_after_a_failed_prior_attempt(
    tmp_path: Path, cool_env: CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> None:
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
  run_cool_sweep(cfg=cfg, now=NOW)

  assert calls == [True, False]


def test_scoped_backend_record_shared_with_live_session_survives(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  db = tmp_path / "opencode.db"
  make_opencode_db(db, {CC_OPENCOLD: {"events": [b"shared"]}})
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))
  write_session_meta(cfg, SID_LIVE, live_meta(cc_session_id=CC_OPENCOLD))

  run_cool_sweep(cfg=cfg, now=NOW, session_id=SID_COLD)

  assert opencode_row_counts(db)["event_sequence"] == 1
  assert opencode_row_counts(db)["event"] == 1


def test_scoped_live_run_does_not_vacuum_unrelated_database_pages(
    tmp_path: Path, cool_env: CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = cool_env
  db = tmp_path / "opencode.db"
  make_opencode_db(db, {CC_OPENORPHAN: {"events": [b"unrelated"]}})
  write_session_meta(cfg, SID_LIVE, live_meta())
  calls: list[bool] = []
  monkeypatch.setattr(
      storage_cool,
      "_vacuum_opencode_db",
      lambda connection, db_path, *, force: calls.append(force),
  )

  run_cool_sweep(cfg=cfg, now=NOW, session_id=SID_LIVE)

  assert calls == []


def test_orphan_thread_metadata_keeps_backend_record_referenced(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  db = tmp_path / "opencode.db"
  make_opencode_db(db, {CC_OPENCOLD: {"events": [b"referenced"]}})
  orphan_thread = cfg.sessions_dir / SID_DEAD / "threads" / TID
  orphan_thread.mkdir(parents=True)
  (orphan_thread / "metadata.json").write_text(json.dumps({"cc_session_id": CC_OPENCOLD}), encoding="utf-8")
  set_opencode_updated(db, CC_OPENCOLD, int((NOW - timedelta(days=3)).timestamp() * 1000))

  run_cool_sweep(cfg=cfg, now=NOW)

  assert opencode_row_counts(db)["event_sequence"] == 1
  assert opencode_row_counts(db)["event"] == 1


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


def test_second_run_frees_nothing(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  _seed_every_category(tmp_path, cfg)

  first = run_cool_sweep(cfg=cfg, now=NOW)
  second = run_cool_sweep(cfg=cfg, now=NOW)

  assert first.total_bytes > 0
  assert second.total_bytes == 0
  assert all(category.count == 0 for category in second.categories)


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


def _unlink_failing_for(blocked: Path):
  real_unlink = Path.unlink

  def failing_unlink(self: Path, *args: object, **kwargs: object) -> None:
    if self == blocked:
      raise PermissionError(f"blocked for test: {self}")
    return real_unlink(self, *args, **kwargs)

  return failing_unlink


def test_unreadable_transport_file_does_not_stop_the_run(
    cool_env: CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta())
  blocked_dir = master_run_dir(cfg, SID_COLD, "2026-08-01T00:00:00+00:00")
  blocked_file = blocked_dir / "agent.raw.ndjson"
  blocked_file.write_bytes(b"blocked")
  other_run = master_run_dir(cfg, SID_COLD, "2026-08-02T00:00:00+00:00")
  (other_run / "stdout.log").write_bytes(b"deletable")
  thread_file = thread_data_dir(cfg, SID_COLD) / "stdout.log"
  thread_file.write_bytes(b"also-deletable")
  monkeypatch.setattr(Path, "unlink", _unlink_failing_for(blocked_file))

  result = run_cool_sweep(cfg=cfg, now=NOW)

  monkeypatch.undo()
  # The blocked file survives; every other deletion lands and the run reports success.
  assert blocked_file.exists()
  assert not (other_run / "stdout.log").exists()
  assert not thread_file.exists()
  assert result.category("raw-transport").count == 2


def test_failing_sql_statement_does_not_stop_the_run(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  db = tmp_path / "opencode.db"
  make_opencode_db(
      db, {
          CC_OPENCOLD: {
              "events": [b"a" * 10],
              "messages": 1
          },
          CC_OPENLIVE: {
              "events": [b"b" * 10],
              "messages": 1
          },
      })
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))
  connection = sqlite3.connect(db)
  try:
    connection.executescript(
        "CREATE TRIGGER block_cold_aggregate BEFORE DELETE ON event "
        f"WHEN OLD.aggregate_id = '{CC_OPENCOLD}' BEGIN SELECT RAISE(ABORT, 'blocked for test'); END;")
    connection.commit()
  finally:
    connection.close()

  result = run_cool_sweep(cfg=cfg, now=NOW)

  counts = opencode_row_counts(db)
  # The failing statement left its aggregate whole; the rest of the sweep still ran.
  assert counts["event_sequence"] == 2
  assert counts["event"] == 2
  assert result.category("opencode-events").count == 0


# ---------------------------------------------------------------------------
# Scoped run (--session)
# ---------------------------------------------------------------------------


def test_session_scope_limits_the_sweep_and_keeps_the_cold_rule(tmp_path: Path, cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta(cc_session_id=CC_OPENCOLD))
  write_session_meta(cfg, SID_LIVE, live_meta(cc_session_id=CC_OPENLIVE))
  (thread_data_dir(cfg, SID_COLD) / "stdout.log").write_bytes(b"cold-transport")
  (thread_data_dir(cfg, SID_LIVE) / "stdout.log").write_bytes(b"live-transport")
  cold_claude = claude_dir(tmp_path, encoded_session_dir(cfg, SID_COLD))
  live_claude = claude_dir(tmp_path, encoded_session_dir(cfg, SID_LIVE))
  orphan_claude = claude_dir(tmp_path, f"-tmp-somehome--charliebot-sessions-{SID_DEAD}")
  age_file(orphan_claude / "transcript.jsonl", timedelta(days=3))
  db = tmp_path / "opencode.db"
  make_opencode_db(
      db, {
          CC_OPENCOLD: {
              "events": [b"a"],
              "messages": 1
          },
          CC_OPENLIVE: {
              "events": [b"b"],
              "messages": 1
          },
      })

  result = run_cool_sweep(cfg=cfg, now=NOW, session_id=SID_COLD)

  assert not (thread_data_dir(cfg, SID_COLD) / "stdout.log").exists()
  assert (thread_data_dir(cfg, SID_LIVE) / "stdout.log").read_bytes() == b"live-transport"
  assert not cold_claude.exists()
  assert live_claude.exists()
  # Orphan reclamation is a whole-sweep concern; a scoped run never reaches it.
  assert orphan_claude.exists()
  counts = opencode_row_counts(db)
  assert counts["event_sequence"] == 1
  assert result.category("opencode-events").count == 1


def test_session_scope_does_not_bypass_the_cold_rule(cool_env: CharlieBotConfig) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_LIVE, live_meta())
  transport = thread_data_dir(cfg, SID_LIVE) / "stdout.log"
  transport.write_bytes(b"live")

  result = run_cool_sweep(cfg=cfg, now=NOW, session_id=SID_LIVE)

  assert transport.read_bytes() == b"live"
  assert result.total_bytes == 0


def test_session_scope_on_unknown_session_fails_loud(cool_env: CharlieBotConfig) -> None:
  with pytest.raises(ValueError, match="session not found"):
    run_cool_sweep(cfg=cool_env, now=NOW, session_id=SID_COLD)


# ---------------------------------------------------------------------------
# CLI and scheduler wiring
# ---------------------------------------------------------------------------


def test_cli_storage_cool_prints_table_and_exits_zero(
    cool_env: CharlieBotConfig, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  cfg = cool_env
  write_session_meta(cfg, SID_COLD, cold_meta())
  (thread_data_dir(cfg, SID_COLD) / "stdout.log").write_bytes(b"transport")
  monkeypatch.setattr(storage_cli, "get_config", lambda: cfg)
  monkeypatch.setattr(sys, "argv", ["charliebot storage", "cool", "--dry-run"])

  storage_cli.main()

  out = capsys.readouterr().out
  assert "raw-transport" in out
  assert "1 files" in out
  report_lines = out.strip().splitlines()[-5:]
  assert len(report_lines) == 5
  assert report_lines[0].startswith("raw-transport")
  assert report_lines[-1].startswith("total")
  assert "dry run" not in out


def test_cli_storage_cool_unknown_session_exits_nonzero(
    cool_env: CharlieBotConfig, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  monkeypatch.setattr(storage_cli, "get_config", lambda: cool_env)
  monkeypatch.setattr(sys, "argv", ["charliebot storage", "cool", "--session", SID_COLD])

  with pytest.raises(SystemExit) as exc_info:
    storage_cli.main()

  assert exc_info.value.code == 1
  assert "session not found" in capsys.readouterr().err


def test_cool_storage_scheduler_handler_runs_the_real_sweep(
    tmp_path: Path, cool_env: CharlieBotConfig, monkeypatch: pytest.MonkeyPatch) -> None:
  cfg = cool_env
  assert "cool_storage" in scheduler_module.TASK_HANDLERS
  write_session_meta(cfg, SID_COLD, cold_meta())
  transport = thread_data_dir(cfg, SID_COLD) / "stdout.log"
  transport.write_bytes(b"transport")
  monkeypatch.setattr(storage_cool, "get_config", lambda: cfg)

  summary = asyncio.run(scheduler_module._cool_storage_handler())

  # The scheduled handler is a real run: the file is gone and the summary says so.
  assert not transport.exists()
  assert "raw-transport 1 files" in summary
  assert summary.startswith("total")
