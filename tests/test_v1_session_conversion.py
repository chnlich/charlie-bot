"""The v1 session conversion script over a scratch home.

The fixture home holds three v1 sessions in the three metadata serializations
real homes carry (one archived, one without a log, one with a legacy key and no
profile key) and one manager root the conversion must leave alone.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

import conftest
import pytest

import scripts.v1_session_conversion as conversion
from src.infra import event_types as ET
from src.runtime import home_writer_fence

ACTIVE_ID = "00000000-0000-4000-8000-00000000000a"
ARCHIVED_ID = "00000000-0000-4000-8000-00000000000b"
NO_LOG_ID = "00000000-0000-4000-8000-00000000000c"
ROOT_ID = "00000000-0000-4000-8000-00000000000d"
V1_IDS = [ACTIVE_ID, ARCHIVED_ID, NO_LOG_ID]
STAMP = "2026-01-01T00:00:00+00:00"

COMPACT_ASCII = {}
COMPACT_UTF8 = {"ensure_ascii": False}
INDENT2_UTF8 = {"ensure_ascii": False, "indent": 2}


def _event(event_id: str, event_type: str, **payload: object) -> dict:
  return {"id": event_id, "type": event_type, "timestamp": STAMP, **payload}


def _write_session(home: Path, meta: dict, dumps_options: dict, events: list[dict] | None) -> None:
  directory = home / "sessions" / meta["id"]
  (directory / "data").mkdir(parents=True)
  (directory / "metadata.json").write_text(json.dumps(meta, **dumps_options), encoding="utf-8")
  if events is not None:
    lines = "".join(json.dumps(e, ensure_ascii=False) + "\n" for e in events)
    (directory / "data" / "chat_events.jsonl").write_text(lines, encoding="utf-8")


@pytest.fixture()
def home(tmp_path: Path) -> Path:
  """A scratch home with three v1 sessions and one manager root."""
  home = conftest.make_home_config(tmp_path).charliebot_home
  _write_session(
      home, {
          "id": ACTIVE_ID,
          "name": "Café notes",
          "status": "active",
          "schema_version": 1,
          "profile": None,
          "created_by_event": None,
          "legacy_key": {
              "kept": "as is"
          },
      }, COMPACT_ASCII, [
          _event("a-1", ET.USER, content="first question"),
          _event("a-2", ET.MASTER_DONE),
          _event("a-3", ET.USER, content="second question, never answered"),
      ])
  _write_session(
      home, {
          "id": ARCHIVED_ID,
          "name": "Old thread",
          "status": "archived",
          "legacy_key": 7
      }, COMPACT_UTF8, [
          _event("b-1", ET.USER, content="done long ago"),
          _event("b-2", ET.MASTER_DONE),
      ])
  _write_session(
      home, {
          "id": NO_LOG_ID,
          "name": "Empty",
          "status": "active",
          "schema_version": 1,
          "profile": None,
          "created_by_event": None,
      }, INDENT2_UTF8, None)
  _write_session(
      home, {
          "id": ROOT_ID,
          "name": "Already a root",
          "status": "active",
          "schema_version": 2,
          "profile": "manager",
          "created_by_event": {
              "session_id": ROOT_ID,
              "event_id": "d-1"
          },
      }, INDENT2_UTF8, [_event("d-1", ET.TASK_CREATED, source_session_id=ROOT_ID, task_parent_id=None)])
  return home


def _tree_hash(root: Path) -> str:
  """One digest over every path and file content under *root*."""
  digest = hashlib.sha256()
  for path in sorted(root.rglob("*")):
    digest.update(str(path.relative_to(root)).encode())
    if path.is_file():
      digest.update(path.read_bytes())
  return digest.hexdigest()


def _state(home: Path) -> str:
  """The digest of everything a conversion writes: the sessions and the receipts, never the fence files."""
  return _tree_hash(home / "sessions") + (_tree_hash(home / "migrations") if (home / "migrations").exists() else "")


def _run(command: str, home: Path) -> int:
  return conversion.main([command, "--home", str(home)])


def _metadata_text(home: Path, session_id: str) -> str:
  return (home / "sessions" / session_id / "metadata.json").read_text(encoding="utf-8")


def _log_ids(home: Path, session_id: str) -> list[str]:
  path = home / "sessions" / session_id / "data" / "chat_events.jsonl"
  return [json.loads(line)["id"] for line in path.read_text(encoding="utf-8").splitlines()]


def test_apply_converts_each_v1_session_and_keeps_the_rest(home: Path, tmp_path: Path) -> None:
  root_before = _metadata_text(home, ROOT_ID)
  originals = {sid: json.loads(_metadata_text(home, sid)) for sid in V1_IDS}

  assert _run("apply", home) == 0

  for session_id, options in zip(V1_IDS, (COMPACT_ASCII, COMPACT_UTF8, INDENT2_UTF8), strict=True):
    expected = {
        **originals[session_id],
        "schema_version": 2,
        "profile": "manager",
        "created_by_event": {
            "session_id": session_id,
            "event_id": conversion.import_event_id(session_id)
        },
    }
    assert _metadata_text(home, session_id) == json.dumps(expected, **options)
  assert _metadata_text(home, ROOT_ID) == root_before
  assert _log_ids(home, ROOT_ID) == ["d-1"]
  assert _log_ids(home, ACTIVE_ID) == ["a-1", "a-2", "a-3", conversion.import_event_id(ACTIVE_ID)]
  assert _log_ids(home, ARCHIVED_ID) == [
      "b-1", "b-2", conversion.import_event_id(ARCHIVED_ID),
      conversion.close_event_id(ARCHIVED_ID)
  ]
  assert _log_ids(home, NO_LOG_ID) == [conversion.import_event_id(NO_LOG_ID)]

  # The runtime reads the converted sessions as task-tree nodes: the old unanswered input is history.
  cfg = conftest.make_home_config(tmp_path)
  tree = conftest.build_task_tree(cfg, conftest.build_session_blocks(cfg))
  assert tree.dispatch.pending_inputs(ACTIVE_ID) == []
  assert [tree.task_state(sid) for sid in V1_IDS] == ["open", "archived", "open"]

  receipt = conversion.load_receipt(home)
  assert receipt["status"] == conversion.RECEIPT_COMPLETE
  assert receipt["counts"] == {"total_sessions": 4, "archived_sessions": 1, "v1_before": 3, "v1_after": 0}
  entries = {entry["session_id"]: entry for entry in receipt["converted"]}
  assert (entries[ACTIVE_ID]["old_schema_version"], entries[ACTIVE_ID]["old_profile"]) == (1, None)
  assert (entries[ARCHIVED_ID]["old_schema_version"], entries[ARCHIVED_ID]["old_profile"]) == (1, None)
  assert (entries[NO_LOG_ID]["old_schema_version"], entries[NO_LOG_ID]["old_profile"]) == (1, None)


def test_second_apply_changes_nothing(home: Path) -> None:
  assert _run("apply", home) == 0
  converted = _state(home)

  assert _run("apply", home) == 0

  assert _state(home) == converted


def test_interrupted_apply_converges_to_the_clean_result(
    home: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  clean_home = conftest.make_home_config(tmp_path / "clean").charliebot_home
  for source in (home / "sessions").iterdir():
    target = clean_home / "sessions" / source.name
    (target / "data").mkdir(parents=True)
    for relative in ("metadata.json", "data/chat_events.jsonl"):
      if (source / relative).exists():
        (target / relative).write_bytes((source / relative).read_bytes())
  assert _run("apply", clean_home) == 0

  append_event = conversion.append_event

  def fail_after_archived_import(session_dir: Path, event: dict) -> None:
    append_event(session_dir, event)
    if session_dir.name == ARCHIVED_ID and event["id"] == conversion.import_event_id(ARCHIVED_ID):
      raise RuntimeError("simulated crash after task_imported")

  monkeypatch.setattr(conversion, "append_event", fail_after_archived_import)
  with pytest.raises(RuntimeError, match="after task_imported"):
    _run("apply", home)
  monkeypatch.undo()
  assert conversion.load_receipt(home)["status"] == conversion.RECEIPT_IN_PROGRESS
  assert json.loads(_metadata_text(home, ARCHIVED_ID)).get("profile") is None
  assert _log_ids(home, ARCHIVED_ID) == ["b-1", "b-2", conversion.import_event_id(ARCHIVED_ID)]

  assert _run("apply", home) == 0

  for session_id in (*V1_IDS, ROOT_ID):
    assert _metadata_text(home, session_id) == _metadata_text(clean_home, session_id)
    assert _log_ids(home, session_id) == _log_ids(clean_home, session_id)
  assert conversion.load_receipt(home)["status"] == conversion.RECEIPT_COMPLETE


def test_rollback_restores_every_byte(home: Path) -> None:
  before = _tree_hash(home / "sessions")
  assert _run("apply", home) == 0
  assert _tree_hash(home / "sessions") != before

  assert _run("rollback", home) == 0

  assert _tree_hash(home / "sessions") == before
  assert not conversion.receipt_path(home).exists()
  assert (home / conversion.ROLLED_BACK_RELATIVE_PATH).is_file()


def test_rollback_after_an_interrupted_apply_restores_every_byte(home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  before = _tree_hash(home / "sessions")
  write_metadata = conversion.write_converted_metadata
  calls: list[str] = []

  def fail_on_the_second_session(scan: conversion.SessionScan, entry: dict) -> None:
    calls.append(scan.session_id)
    if len(calls) == 2:
      raise RuntimeError("simulated crash after the appends")
    write_metadata(scan, entry)

  monkeypatch.setattr(conversion, "write_converted_metadata", fail_on_the_second_session)
  with pytest.raises(RuntimeError, match="simulated crash"):
    _run("apply", home)
  monkeypatch.undo()

  assert _run("rollback", home) == 0

  assert _tree_hash(home / "sessions") == before


def test_rollback_keeps_events_written_after_the_conversion(home: Path) -> None:
  assert _run("apply", home) == 0
  later = _event("a-later", ET.USER, content="written after the conversion")
  log = home / "sessions" / ACTIVE_ID / "data" / "chat_events.jsonl"
  with log.open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(later) + "\n")

  assert _run("rollback", home) == 0

  assert _log_ids(home, ACTIVE_ID) == ["a-1", "a-2", "a-3", "a-later"]


def test_apply_and_rollback_refuse_while_another_process_holds_the_fence(
    home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  before = _state(home)
  with monkeypatch.context() as patcher:
    patcher.setattr(conversion, "source_sha", lambda: pytest.fail("apply looked up git before fencing the home"))
    with home_writer_fence.acquire_home_writer_fence(home, purpose="test holder"):
      assert _run("apply", home) == 1
  refusal = capsys.readouterr().err
  assert "home writer fence held by pid" in refusal
  assert "purpose 'test holder'" in refusal
  assert _state(home) == before
  assert not conversion.receipt_path(home).exists()
  assert _run("apply", home) == 0
  converted = _state(home)

  with home_writer_fence.acquire_home_writer_fence(home, purpose="test holder"):
    assert _run("rollback", home) == 1
  assert _state(home) == converted


def test_apply_refuses_a_home_with_an_unparseable_session_and_writes_nothing(home: Path) -> None:
  log = home / "sessions" / ACTIVE_ID / "data" / "chat_events.jsonl"
  with log.open("a", encoding="utf-8") as stream:
    stream.write('{"id": "torn", "type": "us')
  before = _state(home)

  assert _run("apply", home) == 1

  assert _state(home) == before


@pytest.mark.parametrize("corrupt_manager_log", [False, True])
def test_dry_run_writes_nothing(home: Path, capsys: pytest.CaptureFixture[str], corrupt_manager_log: bool) -> None:
  if corrupt_manager_log:
    manager_log = home / "sessions" / ROOT_ID / "data" / "chat_events.jsonl"
    with manager_log.open("a", encoding="utf-8") as stream:
      stream.write('{"id":"torn","type":"user"')
  before = _tree_hash(home)

  assert _run("dry-run", home) == (1 if corrupt_manager_log else 0)

  assert _tree_hash(home) == before
  assert not (home / home_writer_fence.STATE_DIR_NAME).exists()
  report = capsys.readouterr().out
  assert all(session_id in report for session_id in V1_IDS)
  assert (ROOT_ID in report) is corrupt_manager_log
  if corrupt_manager_log:
    assert "not valid JSON" in report
