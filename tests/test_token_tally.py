"""Tests for the per-model token usage tally (src/core/token_tally.py).

Each test builds fixture log directories under tmp_path and points the collector at them directly,
so no test reads the real home directory. Every assertion checks a named mechanism rather than a
hard-coded total.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path

from src.core.token_tally import collect_token_usage

NAME = "claude-model"


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


def _write_opencode(path: Path, rows: list[tuple[dict, str, str]]) -> None:
  con = sqlite3.connect(path)
  con.execute("create table message (data text)")
  for payload, model_id, provider in rows:
    data = {"role": "assistant", "modelID": model_id, "providerID": provider, "tokens": payload}
    con.execute("insert into message (data) values (?)", (json.dumps(data),))
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
