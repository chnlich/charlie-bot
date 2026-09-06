"""Tests for the per-model token usage tally (src/core/token_tally.py).

Each test builds fixture log directories under tmp_path and points the collector at them directly,
so no test reads the real home directory. Every assertion checks a named mechanism rather than a
hard-coded total.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

import pytest

from src.core import token_tally as tt
from src.core.token_tally import collect_token_usage

NAME = "claude-model"


@pytest.fixture(autouse=True)
def _clear_aggregate_memo() -> None:
  tt._reset_aggregate_memo()
  yield
  tt._reset_aggregate_memo()


def _claude_record(record_id: str, model: str, ts: str, usage: dict) -> dict:
  return {
      "message": {"id": record_id, "model": model, "usage": usage},
      "requestId": f"req-{record_id}",
      "uuid": f"u-{record_id}",
      "timestamp": ts,
  }


class Claude:
  def __init__(self, tmp_path: Path):
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
  def __init__(self, tmp_path: Path):
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
  return {
      "type": "event_msg",
      "timestamp": ts,
      "payload": {"type": "token_count", "info": {"last_token_usage": last, "total_token_usage": total}},
  }


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
    tu = con.execute(
        "select coalesce(max(time_updated), 1699999999999) + 1 from message").fetchone()[0]
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
        "values (?, 'sess', ?, ?, ?)",
        (f"msg-{tu}", tu, tu, blob))


def _write_opencode(path: Path, rows: list[tuple[dict, str, str]]) -> None:
  con = sqlite3.connect(path)
  _create_message_table(con)
  _insert_opencode_raw(con, [({}, row) for row in rows])
  con.commit()
  con.close()


def _collect(claude: Claude | None, codex: Codex | None, db: Path, cache: Path | None = None):
  return collect_token_usage(
      claude_homes=claude.dirs if claude else {},
      codex_homes=codex.homes if codex else {},
      opencode_db=db,
      cache_path=cache,
  )


def _row(tally, source: str, model: str):
  return next(r for r in tally.rows if r.source == source and r.model == model)


def test_tally_is_absolutely_correct(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  codex = Codex(tmp_path)
  usage = {"input_tokens": 100, "cache_creation_input_tokens": 20,
           "cache_read_input_tokens": 40, "output_tokens": 30}
  claude.write(claude.work, "sess1", [_claude_record("m1-id", NAME, "2024-01-01T00:00:00Z", usage)])
  # Replay of the same message in the ext dir with a new session id: deduped, not double counted.
  claude.write(claude.ext, "sess1", [_claude_record("m1-id", NAME, "2024-01-01T00:00:00Z", usage)])
  # A subagent file whose session dir is its parent's; one extra response.
  claude.write(claude.work, "sess2", [], subagents=[
      [_claude_record("sub-id", NAME, "2024-01-02T00:00:00Z",
                      {"input_tokens": 50, "cache_creation_input_tokens": 10,
                       "cache_read_input_tokens": 0, "output_tokens": 5})],
  ])
  # Codex: one rollout.
  codex.write("rollout", [
      _codex_meta(),
      _codex_turn("codex-some"),
      _codex_count({"input_tokens": 60, "cached_input_tokens": 20, "output_tokens": 7},
                   {"total_tokens": 47}, "2024-01-03T00:00:00Z"),
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
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z",
                     {"input_tokens": 10, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0, "output_tokens": 5}),
  ])
  db = tmp_path / "db.sqlite"
  first = _collect(claude, None, db)
  before = _row(first, "Claude Code", NAME)

  # A later session file records the same model: its total rises by exactly those tokens.
  claude.write(claude.work, "sess2", [
      _claude_record("m2", NAME, "2024-01-02T00:00:00Z",
                     {"input_tokens": 1000, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0, "output_tokens": 2}),
  ])
  second = _collect(claude, None, db)
  after = _row(second, "Claude Code", NAME)
  assert after.total == before.total + 1002
  assert after.calls == before.calls + 1


def test_replays_are_not_double_counted(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z",
                     {"input_tokens": 100, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0, "output_tokens": 10}),
  ])
  db = tmp_path / "db.sqlite"
  before = _row(_collect(claude, None, db), "Claude Code", NAME)
  before_total, before_calls = before.total, before.calls

  # Copy the session file verbatim to a new session id (resume/fork behaviour).
  src = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  text = src.read_text()
  dst = claude.work / "projects" / "rel" / "sess2"
  dst.mkdir(parents=True, exist_ok=True)
  (dst / "sess2.jsonl").write_text(text)

  after = _row(_collect(claude, None, db), "Claude Code", NAME)
  assert after.total == before_total
  assert after.calls == before_calls


def test_subagent_files_are_counted(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sessA", [], subagents=[
      [_claude_record("sub1", NAME, "2024-01-01T00:00:00Z",
                      {"input_tokens": 30, "cache_creation_input_tokens": 0,
                       "cache_read_input_tokens": 0, "output_tokens": 3})],
  ])
  tally = _collect(claude, None, tmp_path / "db.sqlite")
  row = _row(tally, "Claude Code", NAME)
  assert row.calls == 1
  assert row.in_fresh == 30
  assert row.output == 3


def test_cross_subscription_merge(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "s1", [
      _claude_record("a", NAME, "2024-01-01T00:00:00Z",
                     {"input_tokens": 10, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0, "output_tokens": 1}),
  ])
  claude.write(claude.ext, "s2", [
      _claude_record("b", NAME, "2024-01-02T00:00:00Z",
                     {"input_tokens": 20, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0, "output_tokens": 2}),
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
  codex.write("parent", [
      _codex_meta(),
      _codex_turn("gpt-p"),
      _codex_count({"input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 2},
                   {"total_tokens": 12}),
      _codex_count({"input_tokens": 20, "cached_input_tokens": 0, "output_tokens": 3},
                   {"total_tokens": 35}),
  ])
  # Subagent whose final total_token_usage already includes the parent's usage (inheritance).
  codex.write("sub", [
      _codex_meta(parent_thread_id="parent"),
      _codex_turn("gpt-sub"),
      _codex_count({"input_tokens": 40, "cached_input_tokens": 0, "output_tokens": 4},
                   {"total_tokens": 35 + 44}),
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
  codex.write("rollout", [
      _codex_meta(),
      _codex_count({"input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 1},
                   {"total_tokens": 11}),
      _codex_turn("gpt-attr"),
  ])
  tally = _collect(None, codex, tmp_path / "db.sqlite")
  assert any(r.source == "Codex" and r.model == "gpt-attr" for r in tally.rows)
  assert not any(r.source == "Codex" and r.model == "unknown" for r in tally.rows)


def test_source_failure_isolation(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "s1", [
      _claude_record("a", NAME, "2024-01-01T00:00:00Z",
                     {"input_tokens": 5, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0, "output_tokens": 1}),
  ])
  # The ext-1 config dir owns a log file that becomes unreadable.
  claude.write(claude.ext, "s1", [
      _claude_record("a2", NAME, "2024-01-01T00:00:00Z",
                     {"input_tokens": 5, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0, "output_tokens": 1}),
  ])
  codex = Codex(tmp_path)
  codex.write("rollout", [
      _codex_meta(),
      _codex_turn("gpt-ok"),
      _codex_count({"input_tokens": 1, "cached_input_tokens": 0, "output_tokens": 1},
                   {"total_tokens": 2}),
  ])
  db = tmp_path / "db.sqlite"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")])
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
  return {"input_tokens": input_, "cache_creation_input_tokens": 0,
          "cache_read_input_tokens": 0, "output_tokens": output}


def test_cache_serves_unchanged_files(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  first = _collect(claude, None, db, cache)
  assert first.scanned_bytes > 0

  second = _collect(claude, None, db, cache)
  # Every log file came from the cache: nothing re-read, same tally.
  assert second.scanned_bytes == 0
  assert _row(second, "Claude Code", NAME).total == _row(first, "Claude Code", NAME).total


def test_cache_replays_are_not_double_counted(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(100, 10))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  before = _row(_collect(claude, None, db, cache), "Claude Code", NAME)

  # A verbatim copy under a new session id (resume/fork) lands after the cache was written.
  src = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  dst = claude.work / "projects" / "rel" / "sess2"
  dst.mkdir(parents=True, exist_ok=True)
  (dst / "sess2.jsonl").write_text(src.read_text())

  after = _row(_collect(claude, None, db, cache), "Claude Code", NAME)
  assert after.total == before.total
  assert after.calls == before.calls


def test_cache_invalidates_on_append(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  before = _row(_collect(claude, None, db, cache), "Claude Code", NAME)

  log_file = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
  with log_file.open("a") as fh:
    fh.write(json.dumps(_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(1000, 2))) + "\n")

  after = _row(_collect(claude, None, db, cache), "Claude Code", NAME)
  assert after.total == before.total + 1002
  assert after.calls == before.calls + 1


def test_corrupt_cache_is_rebuilt_with_note(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  cache.write_text("{ not json")

  tally = _collect(claude, None, db, cache)
  assert any("rebuilt" in n for n in tally.notes)
  assert _row(tally, "Claude Code", NAME).total == 15


def test_codex_cache_serves_unchanged_files(tmp_path: Path) -> None:
  codex = Codex(tmp_path)
  codex.write("rollout", [
      _codex_meta(),
      _codex_turn("gpt-c"),
      _codex_count({"input_tokens": 60, "cached_input_tokens": 20, "output_tokens": 7},
                   {"total_tokens": 47}, "2024-01-03T00:00:00Z"),
  ])
  # A subagent file rides the same cache (its self-check pair is None).
  codex.write("sub", [
      _codex_meta(parent_thread_id="rollout"),
      _codex_turn("gpt-c"),
      _codex_count({"input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 1},
                   {"total_tokens": 47}),
  ])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  first = _collect(None, codex, db, cache)
  assert first.scanned_bytes > 0

  second = _collect(None, codex, db, cache)
  # Every rollout came from the cache: nothing re-read, same tally and self-check note.
  assert second.scanned_bytes == 0
  assert _row(second, "Codex", "gpt-c").total == _row(first, "Codex", "gpt-c").total
  assert ([n for n in second.notes if n.startswith("Codex")] ==
          [n for n in first.notes if n.startswith("Codex")])


def test_codex_cache_invalidates_on_append(tmp_path: Path) -> None:
  codex = Codex(tmp_path)
  codex.write("rollout", [
      _codex_meta(),
      _codex_turn("gpt-a"),
      _codex_count({"input_tokens": 40, "cached_input_tokens": 0, "output_tokens": 3},
                   {"total_tokens": 43}),
  ])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  before = _row(_collect(None, codex, db, cache), "Codex", "gpt-a")

  rollout = codex.home / "sessions" / "rollout" / "rollout.jsonl"
  with rollout.open("a") as fh:
    fh.write(json.dumps(_codex_count(
        {"input_tokens": 100, "cached_input_tokens": 10, "output_tokens": 2},
        {"total_tokens": 155})) + "\n")

  after = _row(_collect(None, codex, db, cache), "Codex", "gpt-a")
  assert after.total == before.total + 102  # appended event: (100 - 10) fresh + 10 read + 2 out
  assert after.calls == before.calls + 1


def test_aggregate_memo_serves_unchanged_walk(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  first = _collect(claude, None, db, cache)

  def boom(path: Path) -> None:
    raise AssertionError("log re-parsed on an aggregate-memo hit")

  monkeypatch.setattr(tt, "_claude_file_contribution", boom)
  second = _collect(claude, None, db, cache)

  assert second.rows == first.rows
  assert second.notes == first.notes
  assert second.scanned_bytes == 0


def test_aggregate_memo_invalidates_on_append(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db = tmp_path / "db.sqlite"
  before = _row(_collect(claude, None, db), "Claude Code", NAME)

  calls = 0
  real = tt._claude_file_contribution

  def spy(path: Path) -> tuple[dict, int]:
    nonlocal calls
    calls += 1
    return real(path)

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
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")])
  first = _collect(claude, None, db, cache)

  def boom(path: Path) -> None:
    raise AssertionError("claude log re-parsed when only the opencode db moved")

  monkeypatch.setattr(tt, "_claude_file_contribution", boom)
  _append_opencode(db, [
      ({"input": 100, "output": 2, "cache": {"read": 0, "write": 0}, "pad": "x" * 5000},
       "oc-m", "prov"),
  ])

  second = _collect(claude, None, db, cache)
  assert _row(second, "Claude Code", NAME).total == _row(first, "Claude Code", NAME).total
  assert _row(second, "opencode", "oc-m").total == 6 + 102


def test_opencode_only_change_is_not_persisted(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")])
  _collect(claude, None, db, cache)
  first_doc = json.loads(cache.read_text())
  assert set(first_doc["sources"]) == {"claude", "opencode"}

  _append_opencode(db, [
      ({"input": 100, "output": 2, "cache": {"read": 0, "write": 0}, "pad": "x" * 5000},
       "oc-m", "prov"),
  ])
  tally = _collect(claude, None, db, cache)
  assert _row(tally, "opencode", "oc-m").total == 6 + 102

  second_doc = json.loads(cache.read_text())
  # The in-process rescan served the fresh tally; the persisted document did not pay for it.
  assert second_doc["sources"] == first_doc["sources"]


def test_non_opencode_change_still_persists(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [
      _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")])
  _collect(claude, None, db, cache)

  claude.write(claude.work, "sess2", [
      _claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(1000, 2))])
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
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")])
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
  assert ([n for n in second.notes if n.startswith("opencode")] ==
          [n for n in first.notes if n.startswith("opencode")])


def test_opencode_scan_row_filters(tmp_path: Path) -> None:
  # Every row shape the LIKE prefilter admits must land exactly where the old fetch-and-parse
  # path put it: counted, skipped as non-contributing, or skipped as malformed.
  db = tmp_path / "db.sqlite"
  con = sqlite3.connect(db)
  _create_message_table(con)
  rows = [
      ({"role": "assistant", "modelID": "oc-full", "providerID": "prov",
        "time": {"created": 1700000000000},
        "tokens": {"input": 10, "output": 2, "cache": {"read": 4, "write": 1}}}, (None, "", "")),
      ({"role": "assistant", "modelID": "/models/oc-local", "providerID": "lmstudio",
        "tokens": {"input": 0, "output": 3, "total": 3}}, (None, "", "")),
      ({"role": "assistant", "modelID": "oc-nocache", "providerID": "prov",
        "tokens": {"input": 5, "output": 1, "total": 6}}, (None, "", "")),
      # skipped rows below: non-assistant role; all counters zero; tokens not an object;
      # malformed JSON that still matches the LIKE prefilter
      ({"role": "user", "modelID": "oc-user",
        "tokens": {"input": 99, "output": 99, "total": 99}}, (None, "", "")),
      ({"role": "assistant", "modelID": "oc-zero",
        "tokens": {"input": 0, "output": 0, "total": 0}}, (None, "", "")),
      ({"role": "assistant", "modelID": "oc-lit", "tokens": "final"}, (None, "", "")),
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
  shapes = [
      ({"role": "assistant", "modelID": "oc-full", "providerID": "prov",
        "time": {"created": 1700000000000},
        "tokens": {"input": 10, "output": 2, "cache": {"read": 4, "write": 1}}},
       (["oc-full", "prov", "2023-11-14T22:13:20+00:00", 10, 1, 4, 2], True)),
      ({"role": "assistant", "modelID": "/models/oc-local", "providerID": "lmstudio",
        "tokens": {"input": 0, "output": 3, "total": 3}},
       (["oc-local (lmstudio)", "lmstudio", None, 0, 0, 0, 3], True)),
      ({"role": "assistant", "modelID": "oc-nocache", "providerID": "prov",
        "tokens": {"input": 5, "output": 1, "total": 6}},
       (["oc-nocache", "prov", None, 5, 0, 0, 1], True)),
      # skipped rows below: non-assistant role counted 0 bytes; zero counters and
      # string tokens pass the filters but project to None with bytes counted
      ({"role": "user", "modelID": "oc-user", "tokens": {"input": 9, "output": 9}},
       (None, False)),
      ({"role": "assistant", "modelID": "oc-zero", "tokens": {"input": 0, "output": 0}},
       (None, True)),
      ({"role": "assistant", "modelID": "oc-lit", "tokens": "final"}, (None, True)),
      ('{"role":"assistant","tokens":{"input": 5,', (None, False)),
      ({"role": "assistant", "modelID": "oc-nano", "tokens": {"input": float("nan")}},
       (None, False)),
  ]
  for data, (rec, counted) in shapes:
    blob = json.dumps(data) if isinstance(data, dict) else data
    got_rec, got_bytes = tt._opencode_row_data(blob)
    assert got_rec == rec
    assert got_bytes == (len(blob) if counted else 0)


def test_opencode_cache_invalidates_on_insert(tmp_path: Path) -> None:
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")])
  before = _row(_collect(None, None, db, cache), "opencode", "oc-m")

  # The pad forces a new db page, so the signature moves on size even if mtime_ns repeats.
  _append_opencode(db, [
      ({"input": 100, "output": 2, "cache": {"read": 0, "write": 0}, "pad": "x" * 5000},
       "oc-m", "prov"),
  ])

  after = _row(_collect(None, None, db, cache), "opencode", "oc-m")
  assert after.total == before.total + 102
  assert after.calls == before.calls + 1


def test_opencode_row_memo_rereads_only_moved_rows(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # Steady state: an append invalidates the file signature, but the row memo re-reads only
  # the new row's blob — the untouched rows' data must not re-enter the parser.
  db, cache = tmp_path / "db.sqlite", tmp_path / "cache.json"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")])
  first = _collect(None, None, db, cache)
  assert first.scanned_bytes > 0

  _append_opencode(db, [
      ({"input": 100, "output": 2, "cache": {"read": 0, "write": 0}, "pad": "x" * 500}, "oc-m", "prov"),
  ])
  projected: list[str] = []
  orig = tt._opencode_row_data

  def spy(data: str) -> tuple[list | None, int]:
    projected.append(data)
    return orig(data)

  monkeypatch.setattr(tt, "_opencode_row_data", spy)
  second = _collect(None, None, db, cache)

  assert len(projected) == 1 and '"input": 100' in projected[0]
  after = _row(second, "opencode", "oc-m")
  assert after.total == _row(first, "opencode", "oc-m").total + 102
  assert after.calls == 2


def test_opencode_row_memo_tracks_in_place_update(tmp_path: Path) -> None:
  # The production write path: assistant rows land with zero tokens and step-finish upserts
  # rewrite data in place with a bumped time_updated; the memo re-reads exactly those rows.
  db = tmp_path / "db.sqlite"
  _write_opencode(db, [({"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}, "oc-m", "prov")])
  before = _row(_collect(None, None, db), "opencode", "oc-m")

  con = sqlite3.connect(db)
  mid, = con.execute("select id from message").fetchone()
  data = {"role": "assistant", "modelID": "oc-m", "providerID": "prov",
          "tokens": {"input": 100, "output": 2, "cache": {"read": 0, "write": 0}}}
  con.execute("update message set data = ?, time_updated = time_updated + 1 where id = ?",
              (json.dumps(data), mid))
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
  msg = {"role": "assistant", "modelID": "oc-m", "providerID": "prov",
         "tokens": {"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}}
  _insert_opencode_raw(con, [(msg, (None, "", ""))])
  con.commit()
  assert (db.parent / "db.sqlite-wal").exists()
  before = _row(_collect(None, None, db, cache), "opencode", "oc-m")

  msg["tokens"] = {"input": 100, "output": 2, "cache": {"read": 0, "write": 0}}
  main_sig = (db.stat().st_mtime_ns, db.stat().st_size)
  _insert_opencode_raw(con, [(msg, (None, "", ""))])
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
  msg = {"role": "assistant", "modelID": "oc-m", "providerID": "prov",
         "tokens": {"input": 5, "output": 1, "cache": {"read": 0, "write": 0}}}
  _insert_opencode_raw(con, [(msg, (None, "", ""))])
  con.commit()
  return db, cache, con


def test_tally_memo_survives_wal_noise_without_row_change(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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


def test_wal_noise_hit_reproves_until_the_next_write(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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


def test_wal_move_with_new_row_still_counts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # A real message row under a moved WAL bumps the epoch, so the proof misses and the
  # collect replays — re-reading only the new row's blob.
  db, cache, con = _wal_db_with_noise_table(tmp_path)
  first = _collect(None, None, db, cache)
  before = _row(first, "opencode", "oc-m")

  _insert_opencode_raw(con, [
      ({}, ({"input": 100, "output": 2, "cache": {"read": 0, "write": 0}, "pad": "x" * 500},
            "oc-m", "prov")),
  ])
  con.commit()

  projected: list[str] = []
  orig = tt._opencode_row_data

  def spy(data: str) -> tuple[list | None, int]:
    projected.append(data)
    return orig(data)

  monkeypatch.setattr(tt, "_opencode_row_data", spy)
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
  _insert_opencode_raw(con, [
      ({"time": {"created": 1700000001000}}, ({"input": 10, "output": 2}, "oc-a", "prov")),
      ({"time": {"created": 1700000002000}}, ({"input": 7, "output": 3}, "oc-b", "prov2")),
      ({"time": {"created": 1700000003000}}, ({"input": 9, "output": 9}, "oc-c", "prov")),
      ({"time": {"created": 1700000004000}}, ({"input": 4, "output": 4}, "oc-d", "prov")),
  ])
  con.commit()
  con.close()
  _collect(None, None, db)

  con = sqlite3.connect(db)
  mid_a = con.execute(
      "select id from message where data like '%\"oc-a\"%' order by time_updated limit 1").fetchone()[0]
  updated = {"role": "assistant", "modelID": "oc-a", "providerID": "prov",
             "time": {"created": 1700000001000},
             "tokens": {"input": 100, "output": 20, "cache": {"read": 4, "write": 2}}}
  con.execute("update message set data = ?, time_updated = time_updated + 1 where id = ?",
              (json.dumps(updated), mid_a))
  moved = {"role": "assistant", "modelID": "oc-b2", "providerID": "prov2",
           "time": {"created": 1700000002000}, "tokens": {"input": 8, "output": 8}}
  con.execute("update message set data = ?, time_updated = time_updated + 1 "
              "where data like '%\"oc-b\"%'", (json.dumps(moved),))
  _insert_opencode_raw(con, [({"time": {"created": 1700000005000}}, ({"input": 1, "output": 1}, "oc-a", "prov"))])
  con.execute("delete from message where data like '%\"oc-a\"%' and id != ?",
              (mid_a,))  # oc-a's max-span row: the model survives, its span must re-derive
  con.execute("delete from message where data like '%\"oc-d\"%'")  # oc-d's only row: bucket empties
  con.commit()
  con.close()
  incremental = _collect(None, None, db)

  tt._opencode_row_memos.clear()
  tt._opencode_partials.clear()
  tt._opencode_row_epochs.clear()
  replay = _collect(None, None, db)

  def opencode_rows(tally):
    return [(r.model, r.calls, r.in_fresh, r.cache_write, r.cache_read, r.output, r.first, r.last,
             [(a.name, a.calls, a.output, a.total) for a in r.accounts])
            for r in tally.rows if r.source == "opencode"]

  assert opencode_rows(incremental) == opencode_rows(replay)
  assert [n for n in incremental.notes if n.startswith("opencode:")] == \
      [n for n in replay.notes if n.startswith("opencode:")]
  assert {r.model for r in incremental.rows if r.source == "opencode"} == {"oc-a", "oc-b2", "oc-c"}
