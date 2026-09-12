"""Tests for the per-model token usage tally (src/core/token_tally.py).

Each test builds fixture log directories under tmp_path and points the collector at them directly,
so no test reads the real home directory. Every assertion checks a named mechanism rather than a
hard-coded total.
"""

from __future__ import annotations

import json
import os
import sqlite3
from collections.abc import Callable
from pathlib import Path

import pytest
from conftest import codex_token_count_event, fresh_state_fixture

from src.core import token_tally as tt
from src.core.token_tally import collect_token_usage

NAME = "claude-model"

_clear_aggregate_memo = fresh_state_fixture(tt._reset_aggregate_memo)


def _claude_record(record_id: str, model: str, ts: str, usage: dict) -> dict:
  return {
      "message": {
          "id": record_id,
          "model": model,
          "usage": usage
      },
      "requestId": f"req-{record_id}",
      "uuid": f"u-{record_id}",
      "timestamp": ts,
  }


class Claude:

  def __init__(self, tmp_path: Path) -> None:
    self.work = tmp_path / ".claude"
    self.ext = tmp_path / ".claude-ext-1"
    self.dirs = {"work (default)": self.work, "ext-1": self.ext}

  def write(self, home: Path, session: str, records: list[dict], subagents=None) -> None:
    sess_dir = home / "projects" / "rel" / session
    sess_dir.mkdir(parents=True, exist_ok=True)
    with (sess_dir / f"{session}.jsonl").open("w") as fh:
      for rec in records:
        fh.write(json.dumps(rec) + "\n")
    if subagents:
      sub_dir = sess_dir / "subagents"
      sub_dir.mkdir(parents=True, exist_ok=True)
      for i, lines in enumerate(subagents):
        with (sub_dir / f"agent-{i}.jsonl").open("w") as fh:
          for line in lines:
            fh.write(json.dumps(line) + "\n")


class Codex:

  def __init__(self, tmp_path: Path) -> None:
    self.home = tmp_path / ".codex"
    self.homes = {"work (default)": self.home}

  def write(self, name: str, records: list[dict]) -> None:
    flow = self.home / "sessions" / name
    flow.mkdir(parents=True, exist_ok=True)
    with (flow / "rollout.jsonl").open("w") as fh:
      for rec in records:
        fh.write(json.dumps(rec) + "\n")


def _codex_meta(**payload) -> dict:
  return {"type": "session_meta", "payload": payload}


def _codex_turn(model: str) -> dict:
  return {"type": "turn_context", "payload": {"model": model}}


def _codex_count(last: dict, total: dict, ts: str = "ts") -> dict:
  return codex_token_count_event(ts, info={"last_token_usage": last, "total_token_usage": total})


def _create_message_table(con: sqlite3.Connection) -> None:
  """The opencode message table's production shape (drizzle schema): the row memo reads the
  id and time_updated audit columns, so fixtures must carry them."""
  con.execute(
      "create table message (id text primary key, session_id text not null, "
      "time_created integer not null, time_updated integer not null, data text not null)")


def _insert_opencode_raw(con: sqlite3.Connection, rows: list[tuple[dict | str, tuple]]) -> None:
  """Insert rows as (data-dict-or-raw-string, account-model-provider payload pair) with
  minted ids and forward-only audit times, mirroring opencode's upsert contract."""
  for data, (payload, model_id, provider) in rows:
    tu = con.execute("select coalesce(max(time_updated), 1699999999999) + 1 from message").fetchone()[0]
    if isinstance(data, dict):
      data.setdefault("role", "assistant")
      data.setdefault("modelID", model_id)
      data.setdefault("providerID", provider)
      data.setdefault("tokens", payload)
      blob = json.dumps(data)
    else:
      blob = data
    con.execute(
        "insert into message (id, session_id, time_created, time_updated, data) "
        "values (?, 'sess', ?, ?, ?)", (f"msg-{tu}", tu, tu, blob))


def _write_opencode(path: Path, rows: list[tuple[dict, str, str]]) -> None:
  con = sqlite3.connect(path)
  _create_message_table(con)
  _insert_opencode_raw(con, [({}, row) for row in rows])
  con.commit()
  con.close()


def _oc_row() -> tuple[dict, str, str]:
  """The file's default opencode row: one assistant message carrying input 5 / output 1."""
  return ({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")


def _padded_opencode_row(pad: int) -> tuple[dict, str, str]:
  """An appended row whose bulk grows the db file, so the signature moves on size
  even when mtime_ns repeats; the tokens stay input 100 / output 2 (total 102)."""
  return ({"input": 100, "output": 2, "cache": {"read": 0, "write": 0}, "pad": "x" * pad}, "oc-m", "prov")


def _spy_row_blobs(monkeypatch: pytest.MonkeyPatch) -> list[str]:
  """Install a blob-read spy on tt._opencode_row_data; returns the projected blobs."""
  projected: list[str] = []
  orig = tt._opencode_row_data

  def spy(data: str) -> tuple[list | None, int]:
    projected.append(data)
    return orig(data)

  monkeypatch.setattr(tt, "_opencode_row_data", spy)
  return projected


def _collect(
    claude: Claude | None, codex: Codex | None, db: Path, cache: Path | None = None, sessions: Path | None = None):
  return collect_token_usage(
      claude_homes=claude.dirs if claude else {},
      codex_homes=codex.homes if codex else {},
      opencode_db=db,
      cache_path=cache,
      sessions_dir=sessions if sessions is not None else db.parent / "sessions",
  )


def _row(tally, source: str, model: str):
  return next(r for r in tally.rows if r.source == source and r.model == model)


def test_tally_is_absolutely_correct(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  codex = Codex(tmp_path)
  usage = {"input_tokens": 100, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 40, "output_tokens": 30}
  claude.write(claude.work, "sess1", [_claude_record("m1-id", NAME, "2024-01-01T00:00:00Z", usage)])
  # Replay of the same message in the ext dir with a new session id: deduped, not double counted.
  claude.write(claude.ext, "sess1", [_claude_record("m1-id", NAME, "2024-01-01T00:00:00Z", usage)])
  # A subagent file whose session dir is its parent's; one extra response.
  claude.write(
      claude.work,
      "sess2", [],
      subagents=[
          [
              _claude_record(
                  "sub-id", NAME, "2024-01-02T00:00:00Z", {
                      "input_tokens": 50,
                      "cache_creation_input_tokens": 10,
                      "cache_read_input_tokens": 0,
                      "output_tokens": 5
                  })
          ],
      ])
  # Codex: one rollout.
  codex.write(
      "rollout", [
          _codex_meta(),
          _codex_turn("codex-some"),
          _codex_count(
              {
                  "input_tokens": 60,
                  "cached_input_tokens": 20,
                  "output_tokens": 7
              }, {"total_tokens": 47}, "2024-01-03T00:00:00Z"),
      ])

  tally = _collect(claude, codex, tmp_path / "db.sqlite")

  cl = _row(tally, "Claude Code", NAME)
  assert cl.calls == 2  # one original + one subagent; the replay is deduped
  assert cl.in_fresh == 100 + 50
  assert cl.cache_write == 20 + 10
  assert cl.cache_read == 40
  assert cl.output == 30 + 5
  assert cl.total == cl.in_fresh + cl.cache_write + cl.cache_read + cl.output
  assert sum(a.total for a in cl.accounts) == cl.total

  cx = _row(tally, "Codex", "codex-some")
  assert cx.in_fresh == 40  # 60 input - 20 cached
  assert cx.cache_read == 20
  assert cx.output == 7
  assert cx.total == 40 + 20 + 7

  assert any(n.startswith("Claude Code") for n in tally.notes)
  assert any(n.startswith("Codex") for n in tally.notes)
  assert any(n.startswith("opencode") for n in tally.notes)


def test_appends_are_visible(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(
      claude.work, "sess1", [
          _claude_record(
              "m1", NAME, "2024-01-01T00:00:00Z", {
                  "input_tokens": 10,
                  "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": 0,
                  "output_tokens": 5
              }),
      ])
  db = tmp_path / "db.sqlite"
  first = _collect(claude, None, db)
  before = _row(first, "Claude Code", NAME)

  # A later session file records the same model: its total rises by exactly those tokens.
  claude.write(
      claude.work, "sess2", [
          _claude_record(
              "m2", NAME, "2024-01-02T00:00:00Z", {
                  "input_tokens": 1000,
                  "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": 0,
                  "output_tokens": 2
              }),
      ])
  second = _collect(claude, None, db)
  after = _row(second, "Claude Code", NAME)
  assert after.total == before.total + 1002
  assert after.calls == before.calls + 1


# Rows are the two stores the replay dedup must hold across: the sqlite row
# store alone, and the row store with the json cache written beside it by the
# first collect.
_REPLAY_STORE_ROWS = [
    pytest.param(False, id="row-store-only"),
    pytest.param(True, id="row-store-plus-cache"),
]


@pytest.mark.parametrize("with_cache", _REPLAY_STORE_ROWS)
def test_replays_are_not_double_counted(tmp_path: Path, with_cache: bool) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(100, 10))])
  db = tmp_path / "db.sqlite"
  cache = tmp_path / "cache.json" if with_cache else None
  before = _row(_collect(claude, None, db, cache), "Claude Code", NAME)

  # Copy the session file verbatim to a new session id (resume/fork behaviour).
  src = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  dst = claude.work / "projects" / "rel" / "sess2"
  dst.mkdir(parents=True, exist_ok=True)
  (dst / "sess2.jsonl").write_text(src.read_text())

  after = _row(_collect(claude, None, db, cache), "Claude Code", NAME)
  assert after.total == before.total
  assert after.calls == before.calls


def test_subagent_files_are_counted(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(
      claude.work,
      "sessA", [],
      subagents=[
          [
              _claude_record(
                  "sub1", NAME, "2024-01-01T00:00:00Z", {
                      "input_tokens": 30,
                      "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0,
                      "output_tokens": 3
                  })
          ],
      ])
  tally = _collect(claude, None, tmp_path / "db.sqlite")
  row = _row(tally, "Claude Code", NAME)
  assert row.calls == 1
  assert row.in_fresh == 30
  assert row.output == 3


def test_cross_subscription_merge(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(
      claude.work, "s1", [
          _claude_record(
              "a", NAME, "2024-01-01T00:00:00Z", {
                  "input_tokens": 10,
                  "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": 0,
                  "output_tokens": 1
              }),
      ])
  claude.write(
      claude.ext, "s2", [
          _claude_record(
              "b", NAME, "2024-01-02T00:00:00Z", {
                  "input_tokens": 20,
                  "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": 0,
                  "output_tokens": 2
              }),
      ])
  tally = _collect(claude, None, tmp_path / "db.sqlite")
  matches = [r for r in tally.rows if r.source == "Claude Code" and r.model == NAME]
  assert len(matches) == 1
  row = matches[0]
  assert row.total == 10 + 1 + 20 + 2
  assert sum(a.total for a in row.accounts) == row.total
  assert {a.name for a in row.accounts} == {"work (default)", "ext-1"}


def test_codex_subagent_no_double_count(tmp_path: Path) -> None:
  codex = Codex(tmp_path)
  codex.write(
      "parent", [
          _codex_meta(),
          _codex_turn("gpt-p"),
          _codex_count({
              "input_tokens": 10,
              "cached_input_tokens": 0,
              "output_tokens": 2
          }, {"total_tokens": 12}),
          _codex_count({
              "input_tokens": 20,
              "cached_input_tokens": 0,
              "output_tokens": 3
          }, {"total_tokens": 35}),
      ])
  # Subagent whose final total_token_usage already includes the parent's usage (inheritance).
  codex.write(
      "sub", [
          _codex_meta(parent_thread_id="parent"),
          _codex_turn("gpt-sub"),
          _codex_count({
              "input_tokens": 40,
              "cached_input_tokens": 0,
              "output_tokens": 4
          }, {"total_tokens": 35 + 44}),
      ])
  tally = _collect(None, codex, tmp_path / "db.sqlite")
  model_rows = [r for r in tally.rows if r.source == "Codex"]
  assert model_rows
  own_sum = (10 - 0) + 2 + (20 - 0) + 3 + (40 - 0) + 4
  assert sum(r.total for r in model_rows) == own_sum
  assert sum(r.total for r in model_rows) < ((12 + 35) + (35 + 44))


def test_codex_model_attribution(tmp_path: Path) -> None:
  codex = Codex(tmp_path)
  # The first token_count precedes the first turn_context in file order.
  codex.write(
      "rollout", [
          _codex_meta(),
          _codex_count({
              "input_tokens": 10,
              "cached_input_tokens": 0,
              "output_tokens": 1
          }, {"total_tokens": 11}),
          _codex_turn("gpt-attr"),
      ])
  tally = _collect(None, codex, tmp_path / "db.sqlite")
  assert any(r.source == "Codex" and r.model == "gpt-attr" for r in tally.rows)
  assert not any(r.source == "Codex" and r.model == "unknown" for r in tally.rows)


def test_source_failure_isolation(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(
      claude.work, "s1", [
          _claude_record(
              "a", NAME, "2024-01-01T00:00:00Z", {
                  "input_tokens": 5,
                  "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": 0,
                  "output_tokens": 1
              }),
      ])
  # The ext-1 config dir owns a log file that becomes unreadable.
  claude.write(
      claude.ext, "s1", [
          _claude_record(
              "a2", NAME, "2024-01-01T00:00:00Z", {
                  "input_tokens": 5,
                  "cache_creation_input_tokens": 0,
                  "cache_read_input_tokens": 0,
                  "output_tokens": 1
              }),
      ])
  codex = Codex(tmp_path)
  codex.write(
      "rollout", [
          _codex_meta(),
          _codex_turn("gpt-ok"),
          _codex_count({
              "input_tokens": 1,
              "cached_input_tokens": 0,
              "output_tokens": 1
          }, {"total_tokens": 2}),
      ])
  db = tmp_path / "db.sqlite"
  _write_opencode(db, [_oc_row()])
  claude.ext.chmod(0o000)  # one config dir unreadable
  try:
    tally = _collect(claude, codex, db)
  finally:
    claude.ext.chmod(0o755)
  assert any(r.source == "Claude Code" and r.model == NAME for r in tally.rows)
  assert any(r.source == "Codex" and r.model == "gpt-ok" for r in tally.rows)
  assert any(r.source == "opencode" for r in tally.rows)
  assert any("unreadable" in n and "ext" in n for n in tally.notes)


def _usage(input_: int, output: int) -> dict:
  return {
      "input_tokens": input_,
      "cache_creation_input_tokens": 0,
      "cache_read_input_tokens": 0,
      "output_tokens": output
  }


def _claude_rig(tmp_path: Path) -> Claude:
  """A Claude home carrying one m1 record in sess1 (usage 10/5): the state most tests start from."""
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  return claude


def test_cache_serves_unchanged_files(tmp_path: Path) -> None:
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  first = _collect(claude, None, db, cache)
  assert first.scanned_bytes > 0

  second = _collect(claude, None, db, cache)
  # Every log file came from the cache: nothing re-read, same tally.
  assert second.scanned_bytes == 0
  assert _row(second, "Claude Code", NAME).total == _row(first, "Claude Code", NAME).total


def test_cache_invalidates_on_append(tmp_path: Path) -> None:
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  before = _row(_collect(claude, None, db, cache), "Claude Code", NAME)

  log_file = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  with log_file.open("a") as fh:
    fh.write(json.dumps(_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(1000, 2))) + "\n")

  after = _row(_collect(claude, None, db, cache), "Claude Code", NAME)
  assert after.total == before.total + 1002
  assert after.calls == before.calls + 1


def test_corrupt_cache_is_rebuilt_with_note(tmp_path: Path) -> None:
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  cache.write_text("{ not json")

  tally = _collect(claude, None, db, cache)
  assert any("rebuilt" in n for n in tally.notes)
  assert _row(tally, "Claude Code", NAME).total == 15


def test_codex_cache_serves_unchanged_files(tmp_path: Path) -> None:
  codex = Codex(tmp_path)
  codex.write(
      "rollout", [
          _codex_meta(),
          _codex_turn("gpt-c"),
          _codex_count(
              {
                  "input_tokens": 60,
                  "cached_input_tokens": 20,
                  "output_tokens": 7
              }, {"total_tokens": 47}, "2024-01-03T00:00:00Z"),
      ])
  # A subagent file rides the same cache (its self-check pair is None).
  codex.write(
      "sub", [
          _codex_meta(parent_thread_id="rollout"),
          _codex_turn("gpt-c"),
          _codex_count({
              "input_tokens": 10,
              "cached_input_tokens": 0,
              "output_tokens": 1
          }, {"total_tokens": 47}),
      ])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  first = _collect(None, codex, db, cache)
  assert first.scanned_bytes > 0

  second = _collect(None, codex, db, cache)
  # Every rollout came from the cache: nothing re-read, same tally and self-check note.
  assert second.scanned_bytes == 0
  assert _row(second, "Codex", "gpt-c").total == _row(first, "Codex", "gpt-c").total
  assert ([n for n in second.notes if n.startswith("Codex")] == [n for n in first.notes if n.startswith("Codex")])


def test_codex_cache_invalidates_on_append(tmp_path: Path) -> None:
  codex = Codex(tmp_path)
  codex.write(
      "rollout", [
          _codex_meta(),
          _codex_turn("gpt-a"),
          _codex_count({
              "input_tokens": 40,
              "cached_input_tokens": 0,
              "output_tokens": 3
          }, {"total_tokens": 43}),
      ])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  before = _row(_collect(None, codex, db, cache), "Codex", "gpt-a")

  rollout = codex.home / "sessions" / "rollout" / "rollout.jsonl"
  with rollout.open("a") as fh:
    fh.write(
        json.dumps(
            _codex_count({
                "input_tokens": 100,
                "cached_input_tokens": 10,
                "output_tokens": 2
            }, {"total_tokens": 155})) + "\n")

  after = _row(_collect(None, codex, db, cache), "Codex", "gpt-a")
  assert after.total == before.total + 102  # appended event: (100 - 10) fresh + 10 read + 2 out
  assert after.calls == before.calls + 1


def test_aggregate_memo_serves_unchanged_walk(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  first = _collect(claude, None, db, cache)

  def boom(path: Path, prev: dict | None = None) -> None:
    raise AssertionError("log re-parsed on an aggregate-memo hit")

  monkeypatch.setattr(tt, "_claude_file_contribution", boom)
  second = _collect(claude, None, db, cache)

  assert second.rows == first.rows
  assert second.notes == first.notes
  assert second.scanned_bytes == 0


def test_aggregate_memo_invalidates_on_append(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  claude = _claude_rig(tmp_path)
  db = tmp_path / "db.sqlite"
  before = _row(_collect(claude, None, db), "Claude Code", NAME)

  calls = 0
  real = tt._claude_file_contribution

  def spy(path: Path, prev: dict | None = None) -> tuple[dict, int]:
    nonlocal calls
    calls += 1
    return real(path, prev)

  monkeypatch.setattr(tt, "_claude_file_contribution", spy)
  log_file = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  with log_file.open("a") as fh:
    fh.write(json.dumps(_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(1000, 2))) + "\n")

  after = _row(_collect(claude, None, db), "Claude Code", NAME)
  assert calls > 0
  assert after.total == before.total + 1002


def test_aggregate_memo_keeps_sources_when_only_opencode_moves(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The host pattern behind the memo: the opencode db's WAL moves under plain serve traffic
  # while the Claude/Codex logs sit unchanged, so the expensive partial must survive.
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  first = _collect(claude, None, db, cache)

  def boom(path: Path, prev: dict | None = None) -> None:
    raise AssertionError("claude log re-parsed when only the opencode db moved")

  monkeypatch.setattr(tt, "_claude_file_contribution", boom)
  _append_opencode(db, [_padded_opencode_row(5000)])

  second = _collect(claude, None, db, cache)
  assert _row(second, "Claude Code", NAME).total == _row(first, "Claude Code", NAME).total
  assert _row(second, "opencode", "oc-m").total == 6 + 102


def test_opencode_only_change_is_not_persisted(tmp_path: Path) -> None:
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  _collect(claude, None, db, cache)
  first_doc = json.loads(cache.read_text())
  assert set(first_doc["sources"]) == {"claude", "opencode"}

  _append_opencode(db, [_padded_opencode_row(5000)])
  tally = _collect(claude, None, db, cache)
  assert _row(tally, "opencode", "oc-m").total == 6 + 102

  second_doc = json.loads(cache.read_text())
  # The in-process rescan served the fresh tally; the persisted document did not pay for it.
  assert second_doc["sources"] == first_doc["sources"]


def test_non_opencode_change_still_persists(tmp_path: Path) -> None:
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  _collect(claude, None, db, cache)

  claude.write(claude.work, "sess2", [_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(1000, 2))])
  _collect(claude, None, db, cache)

  doc = json.loads(cache.read_text())
  sess2 = str(claude.work / "projects" / "rel" / "sess2" / "sess2.jsonl")
  assert sess2 in doc["sources"]["claude"]


def _append_opencode(path: Path, rows: list[tuple[dict, str, str]]) -> None:
  con = sqlite3.connect(path)
  _insert_opencode_raw(con, [({}, row) for row in rows])
  con.commit()
  con.close()


def test_tally_memo_serves_unchanged_collect(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  first = _collect(claude, None, db, cache)

  def boom(*args, **kwargs) -> None:
    raise AssertionError("whole-tally memo hit re-touched the cache document or the db")

  monkeypatch.setattr(tt.TallyCache, "load", boom)
  monkeypatch.setattr(tt.sqlite3, "connect", boom)
  second = _collect(claude, None, db, cache)

  assert second.rows == first.rows
  assert second.notes == first.notes
  assert second.scanned_bytes == 0


def test_opencode_cache_serves_unchanged_db(tmp_path: Path) -> None:
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 4, "write": 2}}, "oc-m", "prov")])
  first = _collect(None, None, db, cache)
  assert first.scanned_bytes > 0

  # Keep the second collect on the persisted-cache path this test names; the whole-tally
  # and aggregate memos would otherwise serve it first.
  tt._reset_aggregate_memo()
  tt._tally_memo = None
  second = _collect(None, None, db, cache)
  # The db contribution came from the cache: nothing re-read, same tally and note.
  assert second.scanned_bytes == 0
  assert _row(second, "opencode", "oc-m").total == _row(first, "opencode", "oc-m").total
  assert ([n for n in second.notes if n.startswith("opencode")] == [n for n in first.notes if n.startswith("opencode")])


# The opencode message shapes the LIKE prefilter admits (assistant role, object tokens, not
# all counters zero). The scan SQL and the incremental _opencode_row_data projection must
# agree on every admitted shape, so the corpus is defined once here and both tests consume
# it; a new admitted shape is added here once.
_ADMITTED_OPENCODE_ROWS = [
    {
        "role": "assistant",
        "modelID": "oc-full",
        "providerID": "prov",
        "time": {
            "created": 1700000000000
        },
        "tokens": {
            "input": 10,
            "output": 2,
            "cache": {
                "read": 4,
                "write": 1
            }
        }
    },
    {
        "role": "assistant",
        "modelID": "/models/oc-local",
        "providerID": "lmstudio",
        "tokens": {
            "input": 0,
            "output": 3,
            "total": 3
        }
    },
    {
        "role": "assistant",
        "modelID": "oc-nocache",
        "providerID": "prov",
        "tokens": {
            "input": 5,
            "output": 1,
            "total": 6
        }
    },
]


def test_opencode_scan_row_filters(tmp_path: Path) -> None:
  # Every row shape the LIKE prefilter admits must land exactly where the old fetch-and-parse
  # path put it: counted, skipped as non-contributing, or skipped as malformed.
  db = tmp_path / "db.sqlite"
  con = sqlite3.connect(db)
  _create_message_table(con)
  rows = [(data, (None, "", "")) for data in _ADMITTED_OPENCODE_ROWS] + [
      # skipped rows below: non-assistant role; all counters zero; tokens not an object;
      # malformed JSON that still matches the LIKE prefilter
      ({
          "role": "user",
          "modelID": "oc-user",
          "tokens": {
              "input": 99,
              "output": 99,
              "total": 99
          }
      }, (None, "", "")),
      ({
          "role": "assistant",
          "modelID": "oc-zero",
          "tokens": {
              "input": 0,
              "output": 0,
              "total": 0
          }
      }, (None, "", "")),
      ({
          "role": "assistant",
          "modelID": "oc-lit",
          "tokens": "final"
      }, (None, "", "")),
      ('{"role":"assistant","tokens":{"input": 5,', (None, "", "")),
  ]
  _insert_opencode_raw(con, rows)
  con.commit()
  con.close()

  tally = _collect(None, None, db)
  by_model = {r.model: r for r in tally.rows if r.source == "opencode"}
  assert set(by_model) == {"oc-full", "oc-local (lmstudio)", "oc-nocache"}
  full = by_model["oc-full"]
  assert (full.in_fresh, full.output) == (10, 2)
  assert (full.cache_read, full.cache_write) == (4, 1)
  assert full.last == "2023-11-14"
  assert by_model["oc-local (lmstudio)"].output == 3


def test_opencode_row_data_matches_the_scan_projection() -> None:
  # The incremental path projects fetched blobs through _opencode_row_data; that projection
  # must agree with the scan SQL on every shape the prefilter admits, including the skips.
  projections = [
      (["oc-full", "prov", "2023-11-14T22:13:20+00:00", 10, 1, 4, 2], True),
      (["oc-local (lmstudio)", "lmstudio", None, 0, 0, 0, 3], True),
      (["oc-nocache", "prov", None, 5, 0, 0, 1], True),
  ]
  # strict=True fails loud when the admitted corpus and its expected projections drift apart.
  shapes = list(zip(_ADMITTED_OPENCODE_ROWS, projections, strict=True)) + [
      # skipped rows below: non-assistant role counted 0 bytes; zero counters and
      # string tokens pass the filters but project to None with bytes counted
      ({
          "role": "user",
          "modelID": "oc-user",
          "tokens": {
              "input": 9,
              "output": 9
          }
      }, (None, False)),
      ({
          "role": "assistant",
          "modelID": "oc-zero",
          "tokens": {
              "input": 0,
              "output": 0
          }
      }, (None, True)),
      ({
          "role": "assistant",
          "modelID": "oc-lit",
          "tokens": "final"
      }, (None, True)),
      ('{"role":"assistant","tokens":{"input": 5,', (None, False)),
      ({
          "role": "assistant",
          "modelID": "oc-nano",
          "tokens": {
              "input": float("nan")
          }
      }, (None, False)),
  ]
  for data, (rec, counted) in shapes:
    blob = json.dumps(data) if isinstance(data, dict) else data
    got_rec, got_bytes = tt._opencode_row_data(blob)
    assert got_rec == rec
    assert got_bytes == (len(blob) if counted else 0)


def test_opencode_cache_invalidates_on_insert(tmp_path: Path) -> None:
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  before = _row(_collect(None, None, db, cache), "opencode", "oc-m")

  _append_opencode(db, [_padded_opencode_row(5000)])

  after = _row(_collect(None, None, db, cache), "opencode", "oc-m")
  assert after.total == before.total + 102
  assert after.calls == before.calls + 1


def test_opencode_row_memo_rereads_only_moved_rows(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # Steady state: an append invalidates the file signature, but the row memo re-reads only
  # the new row's blob — the untouched rows' data must not re-enter the parser.
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  first = _collect(None, None, db, cache)
  assert first.scanned_bytes > 0

  _append_opencode(db, [_padded_opencode_row(500)])
  projected = _spy_row_blobs(monkeypatch)
  second = _collect(None, None, db, cache)

  assert len(projected) == 1 and '"input": 100' in projected[0]
  after = _row(second, "opencode", "oc-m")
  assert after.total == _row(first, "opencode", "oc-m").total + 102
  assert after.calls == 2


def test_opencode_row_memo_tracks_in_place_update(tmp_path: Path) -> None:
  # The production write path: assistant rows land with zero tokens and step-finish upserts
  # rewrite data in place with a bumped time_updated; the memo re-reads exactly those rows.
  db = tmp_path / "db.sqlite"
  _write_opencode(db, [_oc_row()])
  before = _row(_collect(None, None, db), "opencode", "oc-m")

  con = sqlite3.connect(db)
  mid, = con.execute("select id from message").fetchone()
  data = {
      "role": "assistant",
      "modelID": "oc-m",
      "providerID": "prov",
      "tokens": {
          "input": 100,
          "output": 2,
          "cache": {
              "read": 0,
              "write": 0
          }
      }
  }
  con.execute("update message set data = ?, time_updated = time_updated + 1 where id = ?", (json.dumps(data), mid))
  con.commit()
  con.close()

  after = _row(_collect(None, None, db), "opencode", "oc-m")
  assert after.total == before.total - 6 + 102
  assert after.calls == before.calls


def test_opencode_cache_invalidates_on_wal_write(tmp_path: Path) -> None:
  # A WAL-mode commit grows the -wal sidecar and leaves the main file untouched; the main
  # file's stat alone can never see it. The writer stays open across the second collect so
  # no checkpoint folds the sidecar into the main file first.
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  con = sqlite3.connect(db)
  con.execute("pragma journal_mode=WAL")
  _create_message_table(con)
  _insert_opencode_raw(con, [({}, _oc_row())])
  con.commit()
  assert (db.parent / "db.sqlite-wal").exists()
  before = _row(_collect(None, None, db, cache), "opencode", "oc-m")

  main_sig = (db.stat().st_mtime_ns, db.stat().st_size)
  _insert_opencode_raw(con, [({}, ({"input": 100, "output": 2, "cache": {"read": 0, "write": 0}}, "oc-m", "prov"))])
  con.commit()
  assert (db.stat().st_mtime_ns, db.stat().st_size) == main_sig

  after = _row(_collect(None, None, db, cache), "opencode", "oc-m")
  con.close()
  assert after.total == before.total + 102
  assert after.calls == before.calls + 1


def _wal_db_with_noise_table(tmp_path: Path) -> tuple[Path, Path, sqlite3.Connection]:
  """A WAL-mode db holding one contributing message row plus a second table the noise
  writes land in — the production shape behind WAL-sidecar signature moves."""
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  con = sqlite3.connect(db)
  con.execute("pragma journal_mode=WAL")
  _create_message_table(con)
  con.execute("create table other (id text primary key, data text not null)")
  _insert_opencode_raw(con, [({}, _oc_row())])
  con.commit()
  return db, cache, con


def test_tally_memo_survives_wal_noise_without_row_change(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # opencode writes other tables under plain serve traffic, so the WAL sidecar moves while
  # the message table sits unchanged. The row memo's key diff proves that and re-serves the
  # memo instead of replaying the db's rows or loading the persisted document.
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  first = _collect(None, None, db, cache)

  con.execute("insert into other values ('noise', 'x')")
  con.commit()

  def no_parse(data: str) -> tuple[list | None, int]:
    raise AssertionError("row blob re-read when the WAL noise touched no message row")

  def no_cache_doc(*args, **kwargs) -> None:
    raise AssertionError("cache document loaded on an epoch-proof hit")

  monkeypatch.setattr(tt, "_opencode_row_data", no_parse)
  monkeypatch.setattr(tt.TallyCache, "load", no_cache_doc)
  second = _collect(None, None, db, cache)

  assert second.rows == first.rows
  assert second.notes == first.notes
  assert second.scanned_bytes == 0
  con.close()


def test_wal_noise_hit_reproves_until_the_next_write(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The epoch-proof hit re-signs the memo at the scan's own signature, so the next collect
  # with a quiet db takes the stat-only fast hit and never opens the db at all.
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  first = _collect(None, None, db, cache)
  con.execute("insert into other values ('noise', 'x')")
  con.commit()
  second = _collect(None, None, db, cache)
  assert second.rows == first.rows

  def boom(*args, **kwargs) -> None:
    raise AssertionError("quiet db reopened on a signature fast hit")

  monkeypatch.setattr(tt.sqlite3, "connect", boom)
  third = _collect(None, None, db, cache)
  assert third.rows == first.rows
  assert third.notes == first.notes
  con.close()


def test_wal_only_source_walk_round_skips_the_document_rewrite(tmp_path: Path) -> None:
  # The changed round's shape (the hourly cron's): the walk ran fresh, the db's WAL moved,
  # no message row did. The stored opencode entry keeps its signature and the document
  # skips the multi-MB rewrite its re-sign would force.
  claude = _claude_rig(tmp_path)
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  _collect(claude, None, db, cache)
  doc_before = cache.read_bytes()

  tt._aggregate_memo = None  # the changed round: the walk memo is gone, no log moved
  tt._tally_memo = None
  con.execute("insert into other values ('noise', 'x')")
  con.commit()
  _collect(claude, None, db, cache)

  assert cache.read_bytes() == doc_before
  con.close()


def test_source_walk_round_persists_moved_opencode_rows(tmp_path: Path) -> None:
  # The counterpart: rows that moved reach the persisted document on the next source-walk
  # round with a fresh signature, so a fresh process serves them without a cold rescan.
  claude = _claude_rig(tmp_path)
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  _collect(claude, None, db, cache)
  sig_before = json.loads(cache.read_text())["sources"]["opencode"][str(db)]["sig"]

  tt._aggregate_memo = None
  tt._tally_memo = None
  _append_opencode(db, [_padded_opencode_row(500)])
  _collect(claude, None, db, cache)

  entry = json.loads(cache.read_text())["sources"]["opencode"][str(db)]
  assert entry["sig"] != sig_before
  rows_in_fresh = sum(row[1][3] for row in entry["rows"].values() if row[1] is not None)
  assert rows_in_fresh == 5 + 100  # in_fresh: both rows' inputs
  con.close()


def test_entry_served_changed_round_adopts_the_partial(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The changed round whose db never moved: the walk ran fresh and the document entry still
  # matches the file, so the merge serves the entry. A served entry's rows are provably the
  # rows the partial sums (a row move would have moved the signature), so the buckets adopt
  # in place of the per-record fold; a process's first entry-served round replays once to
  # build the partial.
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 4, "write": 2}}, "oc-m", "prov")])
  cold = _collect(claude, None, db, cache)

  replays: list[int] = []
  orig_replay = tt._replay_opencode_records

  def spy_replay(t: tt._Tally, records: list) -> None:
    replays.append(len(records))
    orig_replay(t, records)

  monkeypatch.setattr(tt, "_replay_opencode_records", spy_replay)

  tt._aggregate_memo = None  # the changed round: the walk memo is gone, nothing moved
  tt._tally_memo = None
  adopted = _collect(claude, None, db, cache)
  assert replays == []  # the scan-path merge built the partial: the buckets adopt
  assert adopted.scanned_bytes == 0
  assert _row(adopted, "opencode", "oc-m").total == _row(cold, "opencode", "oc-m").total
  assert [n for n in adopted.notes if n.startswith("opencode")] == \
      [n for n in cold.notes if n.startswith("opencode")]

  tt._opencode_partials.clear()  # a process start: the stored partial serves without a replay
  tt._aggregate_memo = None
  tt._tally_memo = None
  rebuilt = _collect(claude, None, db, cache)
  assert replays == []  # the entry's stored partial adopts; no record folds
  assert _row(rebuilt, "opencode", "oc-m").total == _row(adopted, "opencode", "oc-m").total

  doc = json.loads(cache.read_text())  # an entry without a stored partial (the v1 shape)
  doc["sources"]["opencode"][str(db)].pop("partial", None)
  cache.write_text(json.dumps(doc))
  tt._reset_aggregate_memo()
  tt._aggregate_memo = None
  tt._tally_memo = None
  replayed = _collect(claude, None, db, cache)
  assert replays == [1]  # one replay builds the partial
  tt._aggregate_memo = None
  tt._tally_memo = None
  after_rebuild = _collect(claude, None, db, cache)
  assert replays == [1]  # the rebuilt partial serves every later entry-served round
  assert _row(after_rebuild, "opencode", "oc-m").total == _row(replayed, "opencode", "oc-m").total


def test_wal_move_with_new_row_still_counts(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # A real message row under a moved WAL bumps the epoch, so the proof misses and the
  # collect replays — re-reading only the new row's blob.
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  first = _collect(None, None, db, cache)
  before = _row(first, "opencode", "oc-m")

  _insert_opencode_raw(con, [({}, _padded_opencode_row(500))])
  con.commit()

  projected = _spy_row_blobs(monkeypatch)
  second = _collect(None, None, db, cache)
  after = _row(second, "opencode", "oc-m")

  assert len(projected) == 1 and '"input": 100' in projected[0]
  assert after.total == before.total + 102
  assert after.calls == before.calls + 1
  con.close()


def test_opencode_incremental_merge_matches_full_replay(tmp_path: Path) -> None:
  # A collect whose scan moved rows adjusts the persisted partial by the scan's deltas; the
  # rows must equal a cold replay over the same memo. One round carries every move shape:
  # an in-place update, an in-place model change, an insert, and two deletes — one the
  # max-span row (span re-derivation), one a model's last record (bucket must vanish, not
  # linger as a zero row the replay never builds).
  db = tmp_path / "db.sqlite"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 2, "write": 1}}, "oc-a", "prov")])
  con = sqlite3.connect(db)
  _insert_opencode_raw(
      con, [
          ({
              "time": {
                  "created": 1700000001000
              }
          }, ({
              "input": 10,
              "output": 2
          }, "oc-a", "prov")),
          ({
              "time": {
                  "created": 1700000002000
              }
          }, ({
              "input": 7,
              "output": 3
          }, "oc-b", "prov2")),
          ({
              "time": {
                  "created": 1700000003000
              }
          }, ({
              "input": 9,
              "output": 9
          }, "oc-c", "prov")),
          ({
              "time": {
                  "created": 1700000004000
              }
          }, ({
              "input": 4,
              "output": 4
          }, "oc-d", "prov")),
      ])
  con.commit()
  con.close()
  _collect(None, None, db)

  con = sqlite3.connect(db)
  mid_a = con.execute("select id from message where data like '%\"oc-a\"%' order by time_updated limit 1").fetchone()[0]
  updated = {
      "role": "assistant",
      "modelID": "oc-a",
      "providerID": "prov",
      "time": {
          "created": 1700000001000
      },
      "tokens": {
          "input": 100,
          "output": 20,
          "cache": {
              "read": 4,
              "write": 2
          }
      }
  }
  con.execute("update message set data = ?, time_updated = time_updated + 1 where id = ?", (json.dumps(updated), mid_a))
  moved = {
      "role": "assistant",
      "modelID": "oc-b2",
      "providerID": "prov2",
      "time": {
          "created": 1700000002000
      },
      "tokens": {
          "input": 8,
          "output": 8
      }
  }
  con.execute(
      "update message set data = ?, time_updated = time_updated + 1 "
      "where data like '%\"oc-b\"%'", (json.dumps(moved),))
  _insert_opencode_raw(con, [({"time": {"created": 1700000005000}}, ({"input": 1, "output": 1}, "oc-a", "prov"))])
  con.execute(
      "delete from message where data like '%\"oc-a\"%' and id != ?",
      (mid_a,))  # oc-a's max-span row: the model survives, its span must re-derive
  con.execute("delete from message where data like '%\"oc-d\"%'")  # oc-d's only row: bucket empties
  con.commit()
  con.close()
  incremental = _collect(None, None, db)

  tt._opencode_row_memos.clear()
  tt._opencode_partials.clear()
  tt._opencode_row_epochs.clear()
  replay = _collect(None, None, db)

  def opencode_rows(tally: tt.TokenTally) -> list:
    return [
        (
            r.model, r.calls, r.in_fresh, r.cache_write, r.cache_read, r.output, r.first, r.last, [
                (a.name, a.calls, a.output, a.total) for a in r.accounts
            ]) for r in tally.rows if r.source == "opencode"
    ]

  assert opencode_rows(incremental) == opencode_rows(replay)
  assert [n for n in incremental.notes if n.startswith("opencode:")] == \
      [n for n in replay.notes if n.startswith("opencode:")]
  assert {r.model for r in incremental.rows if r.source == "opencode"} == {"oc-a", "oc-b2", "oc-c"}


def test_row_memo_probe_skips_the_key_scan_on_wal_noise(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The proof aggregates (count, sum of time_updated) answer the WAL-noise round without
  # the per-row key read: a WAL write that touched no message row skips the scan entirely.
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  first = _collect(None, None, db, cache)

  con.execute("insert into other values ('noise2', 'x')")
  con.commit()

  def boom(*args, **kwargs) -> None:
    raise AssertionError("key scan ran although the proof aggregates saw no message row move")

  monkeypatch.setattr(tt, "_scan_opencode_rows", boom)
  second = _collect(None, None, db, cache)
  assert second.rows == first.rows
  assert second.notes == first.notes
  con.close()


def test_cache_document_parses_once_per_process(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The parsed document memoizes per cache path: a changed round re-parses zero document
  # bytes; the per-file signature still forces the moved file's own re-read.
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  _collect(claude, None, db, cache)

  loads = []
  orig = tt.TallyCache.load

  def spy(path: Path, notes: list) -> tt.TallyCache:
    loads.append(path)
    return orig(path, notes)

  monkeypatch.setattr(tt.TallyCache, "load", staticmethod(spy))
  os.utime(claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl", None)  # signature moves
  second = _collect(claude, None, db, cache)
  assert loads == []  # the memoized document served the changed round
  assert _row(second, "Claude Code", NAME).calls == 1  # the re-read file still tallies once


def test_reset_drops_the_probe_and_document_memos(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # _reset_aggregate_memo owns every process-wide memo the collection adds; after it, a
  # fresh round re-parses the document and re-runs the row scan from an empty memo.
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  _collect(None, None, db, cache)

  tt._reset_aggregate_memo()
  assert tt._opencode_doc_synced == {}
  _append_opencode(db, [_padded_opencode_row(100)])

  scans, loads = [], []
  orig_scan = tt._scan_opencode_rows
  orig_load = tt.TallyCache.load

  def spy_scan(con: sqlite3.Connection, memo: dict) -> tuple:
    scans.append(1)
    return orig_scan(con, memo)

  def spy_load(path: Path, notes: list) -> tt.TallyCache:
    loads.append(path)
    return orig_load(path, notes)

  monkeypatch.setattr(tt, "_scan_opencode_rows", spy_scan)
  monkeypatch.setattr(tt.TallyCache, "load", staticmethod(spy_load))
  _collect(None, None, db, cache)
  assert scans == [1]  # empty row memo: the cold scan ran
  assert loads == [cache]  # empty document memo: the document re-parsed


def _cold_reference(collect: Callable[[], tt.TokenTally]) -> tt.TokenTally:
  """Run one collect on reset state, then restore the caller sequence's partial state."""
  saved = (
      dict(tt._source_partials), dict(tt._claude_key_counts), dict(tt._claude_key_records), dict(tt._claude_key_loc), {
          k: set(v) for k, v in tt._claude_key_holders.items()
      }, dict(tt._claude_orphan), tt._aggregate_memo, tt._tally_memo)
  tt._reset_aggregate_memo()
  try:
    return collect()
  finally:
    (
        tt._source_partials, tt._claude_key_counts, tt._claude_key_records, tt._claude_key_loc, tt._claude_key_holders,
        tt._claude_orphan, tt._aggregate_memo, tt._tally_memo) = saved


def _tally_snapshot(tally: tt.TokenTally) -> tuple[list, list]:
  rows = sorted(
      (
          r.source, r.model, r.calls, r.in_fresh, r.cache_write, r.cache_read, r.output, r.first, r.last,
          tuple(sorted((a.name, a.calls, a.total) for a in r.accounts))) for r in tally.rows)
  return rows, sorted(tally.notes)


def test_incremental_partials_match_a_fresh_fold(tmp_path: Path) -> None:
  """Every reconcile round serves what a cold parse+fold of the current corpus serves: appends,
  an earlier-walked newcomer taking a replayed key's credit, a contributor dropping a key
  (orphan transfer), the last copy dropping, a deletion, a relabel and a cacheless round."""
  work, ext = tmp_path / ".claude", tmp_path / ".claude-ext-1"
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"

  def write(home: Path, session: str, records: list[dict]) -> None:
    d = home / "projects" / "rel" / session
    d.mkdir(parents=True, exist_ok=True)
    (d / f"{session}.jsonl").write_text("".join(json.dumps(r) + "\n" for r in records))

  def collect(label: str, homes: dict, with_cache: bool = True) -> None:
    args = {
        "claude_homes": homes,
        "codex_homes": {},
        "opencode_db": db,
        "cache_path": cache if with_cache else None,
        "sessions_dir": tmp_path / "sessions",
    }
    inc = collect_token_usage(**args)
    ref = _cold_reference(lambda: collect_token_usage(**{**args, "cache_path": tmp_path / "ref-cache.json"}))
    assert _tally_snapshot(inc) == _tally_snapshot(ref), label

  def rec(rid: str, ts: str, input_: int) -> dict:
    return {
        "message":
            {
                "id": rid,
                "model": NAME,
                "usage":
                    {
                        "input_tokens": input_,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "output_tokens": 1
                    }
            },
        "timestamp": ts
    }

  both = {"work (default)": work, "ext-1": ext}
  write(work, "s1", [rec("k1", "t1", 100)])
  write(ext, "s2", [rec("k1", "t1", 100), rec("k2", "t2", 200)])
  collect("cold corpus", both)
  write(work, "s1", [rec("k1", "t1", 100), rec("k3", "t3", 300)])
  collect("append to the contributor", both)
  write(work, "s1", [rec("k1", "t1", 100), rec("k3", "t3", 300), rec("k2", "t2", 200)])
  collect("earlier home replays a key the later home holds (credit transfer)", both)
  write(work, "s1", [rec("k3", "t3", 300)])
  collect("contributor drops a replayed key (orphan transfer)", both)
  write(ext, "s2", [rec("k2", "t2", 200)])
  collect("last copy of the key drops", both)
  (ext / "projects" / "rel" / "s2" / "s2.jsonl").unlink()
  collect("file deleted", both)
  collect("account relabel", {"main": work})
  collect("cacheless round", both, with_cache=False)
  collect("cached again", both)


def test_append_tail_claude_parity(tmp_path: Path) -> None:
  """An appended tail parses only the tail: rows, dupes and the entry match a full re-parse."""
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _collect(claude, None, db, cache)
  log_file = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  with log_file.open("a") as fh:
    fh.write(json.dumps(_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(100, 2))) + "\n")
    fh.write(json.dumps(_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))) + "\n")
  after = _collect(claude, None, db, cache)
  reference = _collect(claude, None, db)  # cacheless full re-parse of the same corpus
  assert after.rows == reference.rows
  assert after.scanned_bytes < log_file.stat().st_size  # the tail round read only the appended lines
  entry = json.loads(cache.read_text())["sources"]["claude"][str(log_file)]
  full = tt._claude_file_contribution(str(log_file))[0]
  assert entry["records"] == full["records"]
  assert entry["dupes"] == full["dupes"] == 1  # the appended replay of the prefix's key
  assert entry["end"] == full["end"] == log_file.stat().st_size
  assert entry["guard"] == full["guard"]


def test_append_tail_codex_parity(tmp_path: Path) -> None:
  """The tail round carries the model context, rootness and self-check state forward."""
  codex = Codex(tmp_path)
  codex.write(
      "rollout",
      [_codex_meta(), _codex_turn("gpt-a"),
       _codex_count({"input_tokens": 10}, {"total_tokens": 11})])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _collect(None, codex, db, cache)
  log_file = codex.home / "sessions" / "rollout" / "rollout.jsonl"
  with log_file.open("a") as fh:
    fh.write(json.dumps(_codex_turn("gpt-b")) + "\n")
    fh.write(json.dumps(_codex_count({"input_tokens": 20}, {"total_tokens": 31})) + "\n")
  after = _collect(None, codex, db, cache)
  reference = _collect(None, codex, db)
  assert after.rows == reference.rows
  assert _row(after, "Codex", "gpt-b").total == 20  # the appended count resolves to the new context
  entry = json.loads(cache.read_text())["sources"]["codex"][str(log_file)]
  full = tt._codex_file_contribution(str(log_file))[0]
  assert entry["records"] == full["records"]
  assert entry["check"] == full["check"] == [30, 31]
  assert entry["model_ctx"] == full["model_ctx"] == "gpt-b"
  assert entry["is_root"] == full["is_root"] is True


def test_append_tail_parses_a_completed_partial_line_once(tmp_path: Path) -> None:
  """A trailing fragment stays unparsed; the round whose tail covers it whole counts it once."""
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _collect(claude, None, db, cache)
  log_file = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  record = json.dumps(_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(100, 2)))
  with log_file.open("a") as fh:
    fh.write(record[:len(record) // 2])  # a mid-write fragment, no newline yet
  mid = _collect(claude, None, db, cache)
  assert len([r for r in mid.rows if r.source == "Claude Code"]) == 1
  assert _row(mid, "Claude Code", NAME).total == 15  # the fragment dropped, prefix served
  with log_file.open("a") as fh:
    fh.write(record[len(record) // 2:] + "\n")
  after = _collect(claude, None, db, cache)
  reference = _collect(claude, None, db)
  assert after.rows == reference.rows
  assert _row(after, "Claude Code", NAME).total == 117


def test_append_tail_rejects_a_replaced_or_shrunk_file(tmp_path: Path) -> None:
  """A rewrite the guard cannot prove — replaced prefix or shrink — re-parses whole."""
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _collect(claude, None, db, cache)
  claude.write(  # whole-file rewrite: early content replaced, last line preserved
      claude.work, "sess1",
      [_claude_record("m9", NAME, "2024-01-03T00:00:00Z", _usage(7, 7)),
       _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  after = _collect(claude, None, db, cache)
  reference = _collect(claude, None, db)
  assert after.rows == reference.rows
  assert _row(after, "Claude Code", NAME).total == 29
  claude.write(claude.work, "sess1", [_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(3, 3))])
  after = _collect(claude, None, db, cache)
  reference = _collect(claude, None, db)
  assert after.rows == reference.rows
  assert _row(after, "Claude Code", NAME).total == 6


def test_append_tail_skips_entries_without_a_guard(tmp_path: Path) -> None:
  """A pre-tail-schema entry (no guard/end) re-parses whole instead of crashing."""
  claude = _claude_rig(tmp_path)
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _collect(claude, None, db, cache)
  doc = json.loads(cache.read_text())
  log_file = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  old_entry = doc["sources"]["claude"][str(log_file)]
  for key in ("guard", "end"):
    del old_entry[key]
  doc["sources"]["claude"][str(log_file)] = old_entry
  cache.write_text(json.dumps(doc))
  with log_file.open("a") as fh:
    fh.write(json.dumps(_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(100, 2))) + "\n")
  after = _collect(claude, None, db, cache)
  reference = _collect(claude, None, db)
  assert after.rows == reference.rows
  assert _row(after, "Claude Code", NAME).total == 117


def test_restart_cold_seeds_the_row_memo_from_the_document(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # A process restart rebuilds the row memo from the document's rows map and proves it with
  # one key pass: the restart-cold round re-reads zero message blobs and zero corpus bytes
  # where the unseeded cold scan re-read every contributing row's data.
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  first = _collect(None, None, db, cache)
  tt._reset_aggregate_memo()

  con.execute("insert into other values ('noise2', 'x')")
  con.commit()  # the WAL moves, so the entry's stored signature misses and the scan path runs

  projected = _spy_row_blobs(monkeypatch)
  second = _collect(None, None, db, cache)
  assert projected == []  # the seeded key diff proved every row unchanged without a blob read
  assert second.scanned_bytes == 0
  assert _tally_snapshot(second) == _tally_snapshot(first)
  con.close()


def test_restart_cold_recounts_only_moved_rows(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The seeded memo diffs the live key set: only rows whose (id, time_updated) moved since the
  # document was written re-enter the parser — one insert and one in-place step-finish upsert,
  # the production write shapes, and both deltas fold out of and into the stored partial.
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  first = _collect(None, None, db, cache)
  tt._reset_aggregate_memo()

  _insert_opencode_raw(con, [({}, _padded_opencode_row(500))])
  mid, = con.execute("select id from message where data not like '%pad%'").fetchone()
  con.execute(
      "update message set data = ?, time_updated = time_updated + 1 where id = ?", (
          json.dumps(
              {
                  "role": "assistant",
                  "modelID": "oc-m",
                  "providerID": "prov",
                  "tokens": {
                      "input": 100,
                      "output": 2,
                      "cache": {
                          "read": 0,
                          "write": 0
                      }
                  }
              }), mid))
  con.commit()

  projected = _spy_row_blobs(monkeypatch)
  second = _collect(None, None, db, cache)
  assert len(projected) == 2  # the moved pair only: the untouched rows' blobs stayed unread
  after = _row(second, "opencode", "oc-m")
  assert after.calls == 2  # the upsert replaced its row's record; the insert added one
  assert after.total == _row(first, "opencode", "oc-m").total - 6 + 102 + 102
  con.close()


def test_stored_partial_adopts_without_replay(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The v2 entry's stored partial serves the buckets without folding the records; the replay
  # builder runs only for an entry that carries no partial (the v1 shape).
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  _collect(None, None, db, cache)

  entry = json.loads(cache.read_text())["sources"]["opencode"][str(db)]
  assert set(entry["partial"]["by_model"]) == {"oc-m"}
  assert entry["partial"]["count"] == 1
  assert "records" not in entry

  tt._reset_aggregate_memo()

  def boom(*args, **kwargs) -> None:
    raise AssertionError("the stored partial's buckets were rebuilt by a record replay")

  monkeypatch.setattr(tt, "_replay_opencode_records", boom)
  served = _collect(None, None, db, cache)
  assert _row(served, "opencode", "oc-m").total == 6


def test_legacy_records_entry_still_serves(tmp_path: Path) -> None:
  # A v1 entry (records list, no rows/partial) serves through the replay path; the first
  # scan-path store rewrites it in the current shape.
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  _collect(None, None, db, cache)

  entry = json.loads(cache.read_text())["sources"]["opencode"][str(db)]
  legacy = {
      "version": 1,
      "sources":
          {
              "opencode":
                  {
                      str(db):
                          {
                              "sig": entry["sig"],
                              "records": [row[1] for row in entry["rows"].values() if row[1] is not None]
                          }
                  }
          }
  }
  cache.write_text(json.dumps(legacy))
  tt._reset_aggregate_memo()

  served = _collect(None, None, db, cache)
  assert _row(served, "opencode", "oc-m").total == 6

  _append_opencode(db, [_padded_opencode_row(500)])
  after = _collect(None, None, db, cache)
  assert _row(after, "opencode", "oc-m").total == 108  # the legacy doc's rows still count right


def test_document_with_nan_literal_rebuilds_with_note(tmp_path: Path) -> None:
  # The document parser rejects the NaN/Infinity extensions stdlib json admits; a corrupted
  # document fails loud into the existing cold-rebuild note instead of half-serving.
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [_oc_row()])
  cache.write_text('{"version": 2, "sources": {"opencode": {"x": {"sig": [], "records": [[1, NaN]]}}}}')

  served = _collect(None, None, db, cache)
  assert _row(served, "opencode", "oc-m").total == 6
  assert any("unreadable" in note for note in served.notes)


# ---------------------------------------------------------------------------
# charlie-bot source (thread event logs + master raw captures)
# ---------------------------------------------------------------------------


class _Option:
  """One config.yaml backend option's shape the tally reads (id / type / model)."""

  def __init__(self, id: str, type: str, model: str | None = None) -> None:
    self.id, self.type, self.model = id, type, model


class _Backends:
  """The ``cfg.backends`` shape the collector reads."""

  def __init__(self, options: list[_Option]) -> None:
    self.options = options


class _Registry:
  """Stand-in for CharlieBotConfig's backend registry in the collector."""

  def __init__(self, *options: _Option) -> None:
    self.backends = _Backends(list(options))


def _stub_registry(monkeypatch: pytest.MonkeyPatch, *options: _Option) -> None:
  monkeypatch.setattr(tt, "get_config", lambda: _Registry(*options))


_CLC_GEMINI = _Option("charlie-code-gemini-3.8-flash", "charlie-code", "openai/gemini-3.8-flash")
_CLC_GLM = _Option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.3-Flash")


class Charliebot:
  """Synthetic charlie-bot corpus: session thread logs and master-run captures."""

  def __init__(self, tmp_path: Path) -> None:
    self.root = tmp_path / "sessions"

  def thread(
      self,
      session: str,
      tid: str,
      backend: str | None = None,
      model: str | None = None,
      results: list[tuple[str, dict]] = [],
      session_ids: list[str] = [],
      meta: bool = True,
  ) -> Path:
    """One thread dir with its metadata.json (unless *meta* is False) and events.jsonl whose
    lines are the bare session-id events followed by result events."""
    data = self.root / session / "threads" / tid / "data"
    data.mkdir(parents=True, exist_ok=True)
    if meta:
      doc: dict = {"id": tid, "session_id": session, "description": "d", "status": "completed"}
      if backend is not None:
        doc["backend"] = backend
      if model is not None:
        doc["model"] = model
      (data.parent / "metadata.json").write_text(json.dumps(doc))
    lines = [{"session_id": sid, "timestamp": "2026-07-01T00:00:00+00:00"} for sid in session_ids]
    lines.extend({"type": "result", "result": "", "usage": usage, "timestamp": ts} for ts, usage in results)
    with (data / "events.jsonl").open("w") as fh:
      for line in lines:
        fh.write(json.dumps(line) + "\n")
    return data / "events.jsonl"

  def master(self, session: str, started: str, lines: list[dict]) -> Path:
    """One master-run capture; *lines* are the raw NDJSON events."""
    run = self.root / session / "data" / "master_runs" / started
    run.mkdir(parents=True, exist_ok=True)
    with (run / "agent.raw.ndjson").open("w") as fh:
      for line in lines:
        fh.write(json.dumps(line) + "\n")
    return run / "agent.raw.ndjson"

  def raw_master(self, session: str, started: str, raw: str) -> Path:
    """One master-run capture written verbatim (for shapes other writers produce)."""
    run = self.root / session / "data" / "master_runs" / started
    run.mkdir(parents=True, exist_ok=True)
    (run / "agent.raw.ndjson").write_text(raw)
    return run / "agent.raw.ndjson"


def _write_rollout(codex_home: Path, sid: str) -> None:
  """A codex rollout file whose name embeds *sid* the way the CLI names them."""
  day = codex_home / "sessions" / "2026" / "09" / "11"
  day.mkdir(parents=True, exist_ok=True)
  (day / f"rollout-2026-09-11T10-00-00-{sid}.jsonl").write_text("{}\n")


def _result_usage(input_: int, output: int, cache_read: int = 0) -> dict:
  return {
      "input_tokens": input_,
      "output_tokens": output,
      "cache_read_input_tokens": cache_read,
      "cache_creation_input_tokens": 0,
  }


def test_charliebot_thread_types(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """charlie-code-type threads count under their backend id; claude/opencode-type threads
  (whose runs the CLI sources already carry) stay out entirely."""
  _stub_registry(monkeypatch, _CLC_GEMINI)
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="charlie-code-gemini-3.8-flash",
      model="openai/gemini-3.8-flash",
      results=[("2026-09-11T22:08:00+00:00", _result_usage(1000, 50))])
  cb.thread(
      "s1",
      "t2",
      backend="claude-sonnet-5",
      model="claude-sonnet-5",
      results=[("2026-09-11T22:08:00+00:00", _result_usage(2000, 20))])
  cb.thread(
      "s1",
      "t3",
      backend="opencode-kimi-k3",
      model="fpt-kimi-k3/moonshotai/Kimi-K3",
      results=[("2026-09-11T22:08:00+00:00", _result_usage(3000, 30))])

  tally = _collect(None, None, tmp_path / "db.sqlite", sessions=cb.root)

  row = _row(tally, "charlie-bot", "gemini-3.8-flash")
  assert row.calls == 1
  # The envelope's numbers go in verbatim: input 1000 + output 50.
  assert row.in_fresh == 1000 and row.cache_read == 0 and row.output == 50
  assert row.total == 1050
  assert [a.name for a in row.accounts] == ["charlie-code-gemini-3.8-flash"]
  assert [(r.model, r.calls) for r in tally.rows if r.source == "charlie-bot"] == [("gemini-3.8-flash", 1)]
  assert any(n == "charlie-bot: 1 usage results (CLC + offline codex)" for n in tally.notes)


def test_charliebot_master_legs(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A CLC-shaped capture counts once, with the context line's model and the registry's
  backend id as the account; captures without the context line (the claude CLI's own
  stream) or without a trailing result contribute nothing."""
  _stub_registry(monkeypatch, _CLC_GLM)
  cb = Charliebot(tmp_path)
  cb.master(
      "s1", "2026-09-11T21:11:10.460207+00:00", [
          {
              "type": "session",
              "session_id": "clc-1"
          },
          {
              "type": "context",
              "step": 1,
              "prompt_tokens": 10,
              "model": "openai/zai-org/GLM-5.3-Flash"
          },
          {
              "type": "thought",
              "step": 1,
              "text": "t"
          },
          {
              "type": "result",
              "completed": True,
              "n_steps": 2,
              "usage": {
                  "n_calls": 3,
                  "input_tokens": 500,
                  "output_tokens": 25
              }
          },
      ])
  # A claude-shaped capture: compact separators, no context line, claude-style result.
  cb.raw_master(
      "s1", "2026-09-10T00:00:00+00:00", '{"type":"system","subtype":"init","session_id":"x"}\n'
      '{"type":"result","usage":{"input_tokens":9,"output_tokens":1}}\n')
  # A CLC shape killed mid-turn: context but no result yet.
  cb.master(
      "s1", "2026-09-09T00:00:00+00:00", [
          {
              "type": "context",
              "step": 1,
              "prompt_tokens": 1,
              "model": "openai/zai-org/GLM-5.3-Flash"
          },
      ])

  tally = _collect(None, None, tmp_path / "db.sqlite", sessions=cb.root)

  row = _row(tally, "charlie-bot", "GLM-5.3-Flash")
  assert row.calls == 1
  assert row.total == 525
  assert [a.name for a in row.accounts] == ["charlie-code-glm53-flash"]  # model matched the registry
  assert row.first == "2026-09-11" and row.last == "2026-09-11"  # ts is the run dir's start time
  assert [(r.model, r.calls) for r in tally.rows if r.source == "charlie-bot"] == [("GLM-5.3-Flash", 1)]


def test_charliebot_master_account_falls_back(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A capture whose model matches no charlie-code backend still counts, under the
  clc-master account, with a note saying so."""
  _stub_registry(monkeypatch, _CLC_GLM)
  cb = Charliebot(tmp_path)
  cb.master(
      "s1", "2026-09-11T21:11:10.460207+00:00", [
          {
              "type": "context",
              "step": 1,
              "prompt_tokens": 1,
              "model": "openai/retired-model-x"
          },
          {
              "type": "result",
              "completed": True,
              "usage": {
                  "input_tokens": 10,
                  "output_tokens": 2
              }
          },
      ])

  tally = _collect(None, None, tmp_path / "db.sqlite", sessions=cb.root)

  row = _row(tally, "charlie-bot", "retired-model-x")
  assert row.calls == 1 and row.total == 12
  assert [a.name for a in row.accounts] == ["clc-master"]
  assert any("clc-master" in n and "1 master results" in n for n in tally.notes)


def test_charliebot_metadata_classification(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A null backend field classifies through the config model; an unknown model and an
  unknown id with an unknown prefix land in the notes instead of any row."""
  _stub_registry(monkeypatch, _CLC_GLM)
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend=None,
      model="openai/zai-org/GLM-5.3-Flash",
      results=[("2026-09-11T20:00:00+00:00", _result_usage(100, 5))])
  cb.thread(
      "s1", "t2", backend=None, model="mystery/model-x", results=[("2026-09-11T20:00:00+00:00", _result_usage(200, 5))])
  cb.thread(
      "s1", "t3", backend="weird-backend-x", model=None, results=[("2026-09-11T20:00:00+00:00", _result_usage(300, 5))])

  tally = _collect(None, None, tmp_path / "db.sqlite", sessions=cb.root)

  row = _row(tally, "charlie-bot", "GLM-5.3-Flash")
  assert row.calls == 1 and row.total == 105
  assert [a.name for a in row.accounts] == ["charlie-code-glm53-flash"]
  assert not any(r.model in ("model-x", "weird-backend-x") for r in tally.rows if r.source == "charlie-bot")
  assert any("weird-backend-x" in n and "1 results not counted" in n for n in tally.notes)
  assert any("thread t2 (no backend id)" in n and "1 results not counted" in n for n in tally.notes)


def test_charliebot_codex_reconciliation(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A codex thread counts only when none of its session ids has a rollout on disk: one id
  on disk skips, an id that only lives in the thread log counts, and a multi-id thread with
  any id on disk skips whole with a note. A retired id not in config.yaml classifies by its
  prefix and names its row after the id's own model segment."""
  _stub_registry(monkeypatch, _Option("codex-gpt-5.6-luna", "codex", "gpt-5.6-luna"))
  codex = Codex(tmp_path)
  cb = Charliebot(tmp_path)
  # Session ids in the rollout-name shape: five dash-separated groups, so the file-name
  # extraction (the id is the last five) sees the same string the event log carries.
  s1, s2, s3 = "01a09201-0000-7000-8000-000000000001", "01a09201-0000-7000-8000-000000000002", \
      "01a09201-0000-7000-8000-000000000003"
  multi_a, multi_b = "01a09201-0000-7000-8000-00000000000a", "01a09201-0000-7000-8000-00000000000b"
  cb.thread(
      "s1",
      "t1",
      backend="codex-gpt-5.6-luna",
      model="gpt-5.6-luna",
      session_ids=[s1],
      results=[("2026-08-01T00:00:00+00:00", _result_usage(1000, 10, cache_read=800))])
  _write_rollout(codex.home, s1)
  cb.thread(
      "s1",
      "t2",
      backend="codex-gpt-5.6-luna",
      model="gpt-5.6-luna",
      session_ids=[s2],
      results=[("2026-08-02T00:00:00+00:00", _result_usage(2000, 20, cache_read=500))])
  cb.thread(
      "s1",
      "t3",
      backend="codex-gpt-5.5-personal",
      model="gpt-5.5",
      session_ids=[s3],
      results=[("2026-08-03T00:00:00+00:00", _result_usage(3000, 30))])
  cb.thread(
      "s1",
      "t4",
      backend="codex-gpt-5.6-luna",
      model="gpt-5.6-luna",
      session_ids=[multi_a, multi_b],
      results=[("2026-08-04T00:00:00+00:00", _result_usage(4000, 40))])
  _write_rollout(codex.home, multi_a)

  tally = _collect(None, codex, tmp_path / "db.sqlite", sessions=cb.root)

  models = {r.model: r for r in tally.rows if r.source == "charlie-bot"}
  assert set(models) == {"gpt-5.6-luna", "gpt-5.5-personal"}
  assert models["gpt-5.6-luna"].calls == 1  # t2 only; t1 and t4 are on disk
  assert models["gpt-5.6-luna"].total == 2520  # envelope buckets verbatim
  assert models["gpt-5.5-personal"].calls == 1  # retired id, classified by prefix
  assert models["gpt-5.5-personal"].accounts[0].name == "codex-gpt-5.5-personal"
  skips = [n for n in tally.notes if n.startswith("charlie-bot: skipped codex thread")]
  assert any("t1" in n and s1 in n for n in skips)
  assert any("t4" in n and multi_a in n for n in skips)
  assert any(n == "charlie-bot: 2 usage results (CLC + offline codex)" for n in tally.notes)


def test_charliebot_missing_metadata_skips_with_note(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  _stub_registry(monkeypatch)
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="charlie-code-gemini-3.8-flash",
      model="openai/gemini-3.8-flash",
      results=[("2026-09-11T22:08:00+00:00", _result_usage(1000, 50))],
      meta=False)

  tally = _collect(None, None, tmp_path / "db.sqlite", sessions=cb.root)

  assert not any(r.source == "charlie-bot" for r in tally.rows)
  assert any(n == "charlie-bot: skipped thread t1: no metadata.json" for n in tally.notes)
  assert any(n == "charlie-bot: 0 usage results (CLC + offline codex)" for n in tally.notes)


def test_charliebot_cache_serves_and_appends(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Unchanged thread logs and captures serve from the document; an appended result event
  shows up through the tail parse, and a trailing master result replaces the stored one."""
  _stub_registry(monkeypatch, _CLC_GEMINI)
  cb = Charliebot(tmp_path)
  events = cb.thread(
      "s1",
      "t1",
      backend="charlie-code-gemini-3.8-flash",
      model="openai/gemini-3.8-flash",
      results=[("2026-09-11T22:08:00+00:00", _result_usage(1000, 50))])
  capture = cb.master(
      "s1", "2026-09-11T21:00:00+00:00", [
          {
              "type": "context",
              "step": 1,
              "prompt_tokens": 1,
              "model": "openai/gemini-3.8-flash"
          },
          {
              "type": "result",
              "completed": True,
              "usage": {
                  "input_tokens": 100,
                  "output_tokens": 5
              }
          },
      ])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  first = _collect(None, None, db, cache, sessions=cb.root)
  assert _row(first, "charlie-bot", "gemini-3.8-flash").calls == 2

  second = _collect(None, None, db, cache, sessions=cb.root)
  assert second.scanned_bytes == 0  # everything served from the document
  assert _row(second, "charlie-bot", "gemini-3.8-flash").total == \
      _row(first, "charlie-bot", "gemini-3.8-flash").total

  with events.open("a") as fh:
    fh.write(
        json.dumps(
            {
                "type": "result",
                "result": "",
                "usage": _result_usage(70, 3),
                "timestamp": "2026-09-12T01:00:00+00:00"
            }) + "\n")
  with capture.open("a") as fh:
    fh.write(
        json.dumps({
            "type": "result",
            "completed": True,
            "usage": {
                "input_tokens": 200,
                "output_tokens": 8
            }
        }) + "\n")
  third = _collect(None, None, db, cache, sessions=cb.root)
  row = _row(third, "charlie-bot", "gemini-3.8-flash")
  assert row.calls == 3  # thread result appended + the capture's replaced trailing result
  # Envelope buckets verbatim: thread 1000+50 and 70+3, the capture's new trailing 200+8.
  assert row.total == (1000 + 50) + (70 + 3) + (200 + 8)
  assert row.accounts[0].calls == 3
  assert 0 < third.scanned_bytes < first.scanned_bytes  # only the two moved files re-parsed


def test_charliebot_walk_errors_become_notes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A thread log unreadable at first parse is a note, and the rest of the corpus still counts.
  (A log that turns unreadable AFTER its entry was cached keeps serving that entry, the same
  contract every per-file source runs under: the stat pair still matches, so the file is
  never reopened.)"""
  _stub_registry(monkeypatch, _CLC_GEMINI)
  cb = Charliebot(tmp_path)
  events = cb.thread(
      "s1",
      "t1",
      backend="charlie-code-gemini-3.8-flash",
      model="openai/gemini-3.8-flash",
      results=[("2026-09-11T22:08:00+00:00", _result_usage(1000, 50))])
  cb.thread(
      "s1",
      "t2",
      backend="charlie-code-gemini-3.8-flash",
      model="openai/gemini-3.8-flash",
      results=[("2026-09-11T22:09:00+00:00", _result_usage(10, 1))])
  events.chmod(0o000)
  try:
    tally = _collect(None, None, tmp_path / "db.sqlite", sessions=cb.root)
  finally:
    events.chmod(0o644)
  assert any("unreadable" in n and "t1" in n for n in tally.notes)
  row = _row(tally, "charlie-bot", "gemini-3.8-flash")
  assert row.calls == 1 and row.total == 11  # only t2 counted
