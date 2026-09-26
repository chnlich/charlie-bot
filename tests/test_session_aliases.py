"""SessionAliasStore contract: stat-signature memoized reads, copy-on-write registration."""

import json
from pathlib import Path

import src.core.session_aliases as sa
from src.core.session_aliases import SessionAliasStore, alias_thread_key


def _write(
    path: Path, *, old_session_ids: dict[str, str] | None = None, old_threads: dict[str, dict] | None = None) -> None:
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text(
      json.dumps(
          {
              "old_session_ids": old_session_ids or {},
              "old_threads": old_threads or {},
          },
          ensure_ascii=False,
          indent=2,
          sort_keys=True),
      encoding="utf-8")


def test_resolve_rereads_only_when_the_file_moves(tmp_path: Path, monkeypatch) -> None:
  store = SessionAliasStore(tmp_path)
  parses = []
  real_load = sa.load_json_meta

  def counting_load(path, event, **kwargs):
    parses.append(path.name)
    return real_load(path, event, **kwargs)

  monkeypatch.setattr(sa, "load_json_meta", counting_load)
  _write(tmp_path / "session_aliases.json", old_threads={"s1/r1": {"session_id": "s1", "run_id": "r1"}})

  assert store.resolve_thread("s1", "r1") == {"session_id": "s1", "run_id": "r1"}
  assert store.resolve_thread("s1", "r1") == {"session_id": "s1", "run_id": "r1"}
  assert parses == ["session_aliases.json"], "an unchanged file must not re-parse"

  _write(
      tmp_path / "session_aliases.json",
      old_threads={
          "s1/r1": {
              "session_id": "s1",
              "run_id": "r1"
          },
          "s1/r2": {
              "session_id": "s1",
              "run_id": "r2"
          }
      })
  assert store.resolve_thread("s1", "r2") == {"session_id": "s1", "run_id": "r2"}
  assert len(parses) == 2, "a moved signature must re-parse"

  (tmp_path / "session_aliases.json").unlink()
  assert store.resolve_thread("s1", "r1") is None


def test_register_leaves_the_shared_read_unmutated(tmp_path: Path) -> None:
  store = SessionAliasStore(tmp_path)
  _write(tmp_path / "session_aliases.json", old_threads={"s1/r1": {"session_id": "s1", "run_id": "r1"}})

  assert store.resolve_thread("s1", "r1") == {"session_id": "s1", "run_id": "r1"}
  shared = store._read()

  store.register_run_thread("s1", "r2")

  assert alias_thread_key(
      "s1", "r2") not in shared["old_threads"], ("the registration must write a copy, not the memo's shared entry")
  assert store.resolve_thread("s1", "r2") == {"session_id": "s1", "run_id": "r2"}
  assert store.resolve_thread("s1", "r1") == {"session_id": "s1", "run_id": "r1"}


def test_malformed_file_answers_empty_per_call(tmp_path: Path) -> None:
  store = SessionAliasStore(tmp_path)
  path = tmp_path / "session_aliases.json"
  path.parent.mkdir(parents=True, exist_ok=True)
  path.write_text("{not json", encoding="utf-8")

  assert store.resolve_session("s1") is None
  assert store.resolve_thread("s1", "r1") is None

  _write(path, old_session_ids={"old": "s1"})
  assert store.resolve_session("old") == "s1"
