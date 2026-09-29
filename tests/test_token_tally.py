"""Tests for the per-model token usage tally (src/core/token_tally.py).

Each test builds fixture log directories under tmp_path and points the collector at them directly,
so no test reads the real home directory. Every assertion checks a named mechanism rather than a
hard-coded total.
"""

from __future__ import annotations

import io
import json
import shutil
import sqlite3
from pathlib import Path
from typing import Any

import pytest
from conftest import codex_token_count_event, fresh_state_fixture

from src.core import token_tally as tt
from src.core.token_tally import collect_token_usage
from src.core.usage_ledger import UsageLedger

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


def _usage(input_: int, output: int) -> dict:
  return {
      "input_tokens": input_,
      "cache_creation_input_tokens": 0,
      "cache_read_input_tokens": 0,
      "output_tokens": output
  }


class Claude:

  def __init__(self, tmp_path: Path) -> None:
    self.work = tmp_path / ".claude"
    self.ext = tmp_path / ".claude-ext-1"
    self.dirs = {"work (default)": self.work, "ext-1": self.ext}

  def write(self, home: Path, session: str, records: list[dict], subagents: list[list[dict]] | None = None) -> None:
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


def _codex_meta(**payload: Any) -> dict:
  return {"type": "session_meta", "payload": payload}


def _codex_turn(model: str) -> dict:
  return {"type": "turn_context", "payload": {"model": model}}


def _codex_count(last: dict, total: dict, ts: str = "ts") -> dict:
  return codex_token_count_event(ts, info={"last_token_usage": last, "total_token_usage": total})


def _db_and_cache(tmp_path: Path) -> tuple[Path, Path]:
  """The scratch on-disk layout every collect round reads: the opencode db and the tally cache."""
  return tmp_path / "db.sqlite", tmp_path / "cache.json"


def _collect(
    claude: Claude | None,
    codex: Codex | None,
    db: Path,
    cache: Path | None = None,
    sessions: Path | None = None) -> tt.TokenTally:
  return collect_token_usage(
      claude_homes=claude.dirs if claude else {},
      codex_homes=codex.homes if codex else {},
      opencode_db=db,
      cache_path=cache,
      sessions_dir=sessions if sessions is not None else db.parent / "sessions",
  )


def _row(tally: tt.TokenTally, source: str, model: str) -> tt.ModelRow:
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
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  db = tmp_path / "db.sqlite"
  first = _collect(claude, None, db)
  before = _row(first, "Claude Code", NAME)

  # A later session file records the same model: its total rises by exactly those tokens.
  claude.write(claude.work, "sess2", [_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(1000, 2))])
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


def _claude_rig(tmp_path: Path) -> Claude:
  """A Claude home carrying one m1 record in sess1 (usage 10/5): the state most tests start from."""
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  return claude


def test_parse_lines_multi_chunk_giant_line_parity(monkeypatch: pytest.MonkeyPatch) -> None:
  """A no-marker observation line spanning many chunks parses once: objects, order and the
  consumed offset match the shapes the per-round re-concat read (the gigabyte raw-log shape
  whose re-concat paid O(line^2 / chunk))."""
  monkeypatch.setattr(tt, "_PARSE_CHUNK", 64)
  lines = [
      b'{"type": "context", "model": "clc-x"}',
      b'{"pad": "' + b"q" * 500 + b'"}',  # no marker, spans ~8 chunks
      b'{"type": "result", "usage": {"input_tokens": 3, "output_tokens": 4}}',
      b'{"pad2": "' + b"r" * 130 + b'"}',  # no marker, spans 2-3 chunks
      b'{"type": "result", "usage": {"input_tokens": 5, "output_tokens": 6}}',
  ]
  blob = b"\n".join(lines) + b"\n" + b'{"trailing": "fragment"}'
  objects, consumed = tt._parse_lines(io.BytesIO(blob), tt._CHARLIEBOT_MASTER_MARKERS)
  assert consumed == blob.rfind(b"\n") + 1  # the trailing fragment stays unconsumed
  assert [(o.get("type"), o.get("usage", {}).get("input_tokens")) for o in objects] == [
      ("context", None), ("result", 3), ("result", 5)
  ]


def test_append_tail_rejects_a_replaced_or_shrunk_file(tmp_path: Path) -> None:
  """A rewrite the guard cannot prove — replaced prefix or shrink — re-parses whole."""
  claude = _claude_rig(tmp_path)
  db, cache = _db_and_cache(tmp_path)
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


# ---------------------------------------------------------------------------
# charlie-bot source (thread event logs + master raw captures)
# ---------------------------------------------------------------------------


class _Option:
  """One config.yaml backend option's shape the tally reads (id / type / model)."""

  def __init__(self, option_id: str, option_type: str, model: str | None = None) -> None:
    self.id, self.type, self.model = option_id, option_type, model


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


class Charliebot:
  """Synthetic charlie-bot corpus: session thread logs and master-run captures."""

  def __init__(self, tmp_path: Path) -> None:
    self.root = tmp_path / "sessions"

  def thread(
      self,
      session: str,
      tid: str,
      results: list[tuple[str, dict]],
      session_ids: list[str],
      backend: str | None,
      model: str | None,
      meta: bool = True,
  ) -> Path:
    """One thread dir with its metadata.json (unless *meta* is False) and events.jsonl whose
    lines are the bare session-id events followed by result events. The two list parameters
    take no default: a mutable default would leak one call's corpus into the next."""
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


def _result_usage(input_: int, output: int, cache_read: int = 0) -> dict:
  return {
      "input_tokens": input_,
      "output_tokens": output,
      "cache_read_input_tokens": cache_read,
      "cache_creation_input_tokens": 0,
  }


def test_charliebot_thread_row_takes_the_recorded_model_first(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A thread's row names the model its metadata recorded even when config maps its id to a
  later version; a thread without a recorded model takes the config model, and a retired id
  without one takes the id minus its type prefix."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"))
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="charlie-code-glm-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      session_ids=[],
      results=[("2026-09-11T20:00:00+00:00", _result_usage(100, 5))])
  cb.thread(
      "s1",
      "t2",
      backend="charlie-code-glm-flash",
      model=None,
      session_ids=[],
      results=[("2026-09-12T20:00:00+00:00", _result_usage(200, 5))])
  cb.thread(
      "s1",
      "t3",
      backend="charlie-code-kimi-k3",
      model=None,
      session_ids=[],
      results=[("2026-09-13T20:00:00+00:00", _result_usage(300, 5))])

  tally = _collect(None, None, tmp_path / "db.sqlite", sessions=cb.root)

  recorded = _row(tally, "charlie-bot", "GLM-5.3-Flash")
  assert recorded.calls == 1 and recorded.total == 105
  assert [a.name for a in recorded.accounts] == ["charlie-code-glm-flash"]
  assert _row(tally, "charlie-bot", "GLM-5.4-Flash").total == 205
  retired = _row(tally, "charlie-bot", "kimi-k3")
  assert retired.total == 305
  assert [a.name for a in retired.accounts] == ["charlie-code-kimi-k3"]


# ---------------------------------------------------------------------------
# usage ledger capture (capture_jsonl_sources)
# ---------------------------------------------------------------------------


def _parity_fixture(tmp_path: Path) -> tuple[Claude, Codex]:
  """The Claude and Codex corpus of test_tally_is_absolutely_correct: an original response,
  its verbatim replay in a second config dir, a subagent response, and one Codex rollout."""
  claude = Claude(tmp_path)
  codex = Codex(tmp_path)
  usage = {"input_tokens": 100, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 40, "output_tokens": 30}
  claude.write(claude.work, "sess1", [_claude_record("m1-id", NAME, "2024-01-01T00:00:00Z", usage)])
  claude.write(claude.ext, "sess1", [_claude_record("m1-id", NAME, "2024-01-01T00:00:00Z", usage)])
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
  return claude, codex


def _capture(claude: Claude | None,
             codex: Codex | None,
             ledger: UsageLedger,
             cache: Path | None = None) -> dict[str, int]:
  """One capture round against *ledger*; *cache*, when given, is loaded and saved like the
  collect's own document, so the capture rides the same cache-gated serve."""
  notes: list[str] = []
  tally_cache = tt.TallyCache.load(cache, notes) if cache is not None else None
  written = tt.capture_jsonl_sources(
      ledger, "host-a", claude.dirs if claude else {}, codex.homes if codex else {}, tally_cache)
  if tally_cache is not None:
    tally_cache.save(cache)
  return written


def _ledger_row(ledger: UsageLedger, source: str, model: str):
  return next(r for r in ledger.model_rows() if r.source == source and r.model == model)


def test_capture_parity_with_the_collect_rows(tmp_path: Path) -> None:
  """Every ledger row equals the matching collect row on source, model, the four token
  fields, calls, first and last: the capture saw exactly what the collect counted."""
  claude, codex = _parity_fixture(tmp_path)
  tally = _collect(claude, codex, tmp_path / "db.sqlite")
  collect_rows = {(r.source, r.model): r for r in tally.rows}
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(claude, codex, ledger)
    ledger_rows = {(r.source, r.model): r for r in ledger.model_rows()}
  assert set(ledger_rows) == set(collect_rows)
  for key, lr in ledger_rows.items():
    cr = collect_rows[key]
    assert (lr.in_fresh, lr.cache_write, lr.cache_read, lr.output) == \
        (cr.in_fresh, cr.cache_write, cr.cache_read, cr.output)
    assert (lr.calls, lr.first, lr.last) == (cr.calls, cr.first, cr.last)


def test_captured_rows_survive_deleting_the_source_files(tmp_path: Path) -> None:
  """Capturing again over deleted sources leaves every ledger row equal field by field:
  the ledger contains no delete, so its rows outlive the logs they were parsed from."""
  claude, codex = _parity_fixture(tmp_path)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(claude, codex, ledger)
    before = ledger.model_rows()
    shutil.rmtree(claude.work)
    shutil.rmtree(claude.ext)
    shutil.rmtree(codex.home)
    _capture(claude, codex, ledger)
    assert ledger.model_rows() == before


def test_second_capture_without_changes_writes_nothing(tmp_path: Path) -> None:
  """A file the ledger already holds at the same signature is skipped: the second capture
  over an unchanged corpus writes zero records."""
  claude, codex = _parity_fixture(tmp_path)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    first = _capture(claude, codex, ledger)
    assert first == {"Claude Code": 3, "Codex": 1}  # every parsed file's records, replays included
    assert _capture(claude, codex, ledger) == {"Claude Code": 0, "Codex": 0}


def test_appended_line_is_captured_and_second_dir_replay_counts_once(tmp_path: Path) -> None:
  """An appended line reaches the ledger on the next capture (the file rewrites whole: its
  captured records re-upsert beside the new one), and a message id replayed into a second
  config dir is captured yet still counted once."""
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  cache = tmp_path / "cache.json"
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(claude, None, ledger, cache)
    before = _ledger_row(ledger, "Claude Code", NAME)

    # Resume/fork behaviour: the session file copied verbatim into the second config dir.
    src = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
    replay_dir = claude.ext / "projects" / "rel" / "sess1"
    replay_dir.mkdir(parents=True)
    (replay_dir / "sess1.jsonl").write_text(src.read_text())
    assert _capture(claude, None, ledger, cache)["Claude Code"] == 1  # the replay file is captured too
    replayed = _ledger_row(ledger, "Claude Code", NAME)
    assert replayed.calls == before.calls == 1  # ... yet its message id counts once

    with src.open("a") as fh:
      fh.write(json.dumps(_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(100, 2))) + "\n")
    written = _capture(claude, None, ledger, cache)
    after = _ledger_row(ledger, "Claude Code", NAME)

  assert written["Claude Code"] == 2  # m1 re-upserts beside the appended m2
  assert after.calls == 2 and after.output == 7
  assert (after.first, after.last) == ("2024-01-01", "2024-01-02")


# ---------------------------------------------------------------------------
# usage ledger capture (capture_charliebot)
# ---------------------------------------------------------------------------


def _capture_charliebot(ledger: UsageLedger, sessions_dir: Path, cache: Path | None = None) -> int:
  """One charlie-bot capture round against *ledger*; *cache* rides like the collect's own
  document, so the capture shares the collect's cache-gated serve."""
  notes: list[str] = []
  tally_cache = tt.TallyCache.load(cache, notes) if cache is not None else None
  written = tt.capture_charliebot(ledger, "host-a", sessions_dir, tally_cache)
  if tally_cache is not None:
    tally_cache.save(cache)
  return written


def _ledger_rows(ledger: UsageLedger) -> dict:
  return {(r.source, r.model): r for r in ledger.model_rows()}


def _codex_rollout(codex: Codex, sid: str, model: str, last: dict, total: dict, ts: str) -> Path:
  """One Codex rollout named for its session id — the name the real homes carry and the
  capture's session registration parses."""
  flow = codex.home / "sessions" / f"rollout-{sid}"
  flow.mkdir(parents=True, exist_ok=True)
  with (flow / f"rollout-{sid}.jsonl").open("w") as fh:
    for line in (_codex_meta(), _codex_turn(model), _codex_count(last, total, ts)):
      fh.write(json.dumps(line) + "\n")
  return flow / f"rollout-{sid}.jsonl"


def test_charliebot_capture_parity_with_the_collect_rows(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """CLC threads plus a CLC master capture give the ledger the same charlie-bot rows
  (source, model, the four token fields, calls) the collect counted."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"))
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="charlie-code-glm-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      session_ids=[],
      results=[("2026-09-11T20:00:00+00:00", _result_usage(100, 5, cache_read=10))])
  cb.thread(
      "s1",
      "t2",
      backend="charlie-code-glm-flash",
      model=None,
      session_ids=[],
      results=[("2026-09-12T20:00:00+00:00", _result_usage(200, 6))])
  cb.master(
      "s1", "20260913T000000Z", [
          {
              "type": "context",
              "model": "openai/zai-org/GLM-5.4-Flash"
          },
          {
              "type": "result",
              "usage": _result_usage(300, 7)
          },
      ])
  cache = tmp_path / "cache.json"

  tally = _collect(None, None, tmp_path / "db.sqlite", cache, sessions=cb.root)
  collect_rows = {(r.source, r.model): r for r in tally.rows if r.source == "charlie-bot"}
  assert set(collect_rows) == {("charlie-bot", "GLM-5.3-Flash"), ("charlie-bot", "GLM-5.4-Flash")}
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root, cache) == 3
    assert _capture_charliebot(ledger, cb.root, cache) == 0  # the unchanged corpus is skipped
    ledger_rows = {(r.source, r.model): r for r in ledger.model_rows() if r.source == "charlie-bot"}

  assert set(ledger_rows) == set(collect_rows)
  for key, lr in ledger_rows.items():
    cr = collect_rows[key]
    assert (lr.in_fresh, lr.cache_write, lr.cache_read, lr.output, lr.calls) == \
        (cr.in_fresh, cr.cache_write, cr.cache_read, cr.output, cr.calls)


def test_charliebot_codex_thread_excluded_by_the_captured_rollout(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A codex-type thread whose session id has a captured rollout is excluded by the ledger's
  any-match rule; deleting the rollout file and re-capturing changes no row — the fallback
  rows stay stored but excluded, and the native rows are never deleted."""
  _stub_registry(monkeypatch, _Option("codex-gpt", "codex", "openai/gpt-5"))
  sid = "rolloutsid1"
  codex = Codex(tmp_path)
  _codex_rollout(
      codex, sid, "codex-gpt5", {
          "input_tokens": 60,
          "cached_input_tokens": 20,
          "output_tokens": 7
      }, {"total_tokens": 47}, "2026-09-10T00:00:00Z")
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="codex-gpt",
      model=None,
      session_ids=[sid],
      results=[("2026-09-11T20:00:00+00:00", _result_usage(100, 5))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture(None, codex, ledger)["Codex"] == 1  # the rollout is captured native
    assert _capture_charliebot(ledger, cb.root) == 1  # the thread's fallback row is stored
    rows = _ledger_rows(ledger)
    assert ("charlie-bot", "gpt-5") not in rows  # excluded: the session id has a native record
    assert rows["Codex", "codex-gpt5"].calls == 1

    shutil.rmtree(codex.home)
    assert _capture(None, codex, ledger)["Codex"] == 0  # the native rows survive the deletion
    assert _capture_charliebot(ledger, cb.root) == 0  # the unchanged thread file is skipped
    assert _ledger_rows(ledger) == rows


def test_charliebot_fallback_rows_count_until_the_cli_log_is_captured(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A codex-type thread with no rollout anywhere and a claude-type thread with no Claude
  transcript are counted as fallback rows; capturing the Claude transcript whose stem the
  thread's session id names retires that row, while the codex thread's row stays."""
  _stub_registry(
      monkeypatch, _Option("codex-gpt", "codex", "openai/gpt-5"),
      _Option("claude-sonnet", "claude", "anthropic/claude-sonnet-4"))
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="codex-gpt",
      model=None,
      session_ids=["orphan-sid"],
      results=[("2026-09-11T20:00:00+00:00", _result_usage(100, 5))])
  cb.thread(
      "s1",
      "t2",
      backend="claude-sonnet",
      model=None,
      session_ids=["cl-sess"],
      results=[("2026-09-12T20:00:00+00:00", _result_usage(200, 6))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root) == 2
    codex_fb = _ledger_rows(ledger)["charlie-bot", "gpt-5"]
    claude_fb = _ledger_rows(ledger)["charlie-bot", "claude-sonnet-4"]
    assert (codex_fb.calls, codex_fb.fallback_calls, codex_fb.in_fresh, codex_fb.output) == (1, 1, 100, 5)
    assert (claude_fb.calls, claude_fb.fallback_calls) == (1, 1)

    claude = Claude(tmp_path)
    claude.write(claude.work, "cl-sess", [_claude_record("m1", NAME, "2026-09-12T20:00:00Z", _usage(200, 6))])
    assert _capture(claude, None, ledger)["Claude Code"] == 1
    rows = _ledger_rows(ledger)
    assert ("charlie-bot", "claude-sonnet-4") not in rows  # excluded: the transcript is captured
    assert rows["charlie-bot", "gpt-5"].fallback_calls == 1  # the codex thread stays counted


def test_charliebot_capture_skips_unkeyable_threads(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A codex-type thread with no session ids (nothing to key the any-match exclusion on) and
  a thread whose backend id neither the registry nor a type prefix can classify contribute
  no records; both files are still recorded as captured so they are never re-parsed."""
  _stub_registry(monkeypatch, _Option("codex-gpt", "codex", "openai/gpt-5"))
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="codex-gpt",
      model=None,
      session_ids=[],
      results=[("2026-09-11T20:00:00+00:00", _result_usage(100, 5))])
  cb.thread(
      "s1",
      "t2",
      backend="mystery-backend",
      model=None,
      session_ids=["sid-x"],
      results=[("2026-09-12T20:00:00+00:00", _result_usage(200, 6))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root) == 0
    assert ledger.model_rows() == []
    assert len(ledger.captured_sigs("host-a")) == 2
    assert _capture_charliebot(ledger, cb.root) == 0


# ---------------------------------------------------------------------------
# usage ledger capture (capture_runs)
# ---------------------------------------------------------------------------


class Runs:
  """Synthetic Run directories: sessions/<sid>/data/runs/<run id>/ with its metadata.json
  and agent.raw.ndjson written directly, the same layout the Run workers keep."""

  def __init__(self, tmp_path: Path) -> None:
    self.root = tmp_path / "sessions"

  def run(
      self,
      session: str,
      run_id: str,
      lines: list[dict],
      backend: str | None = None,
      model: str | None = None,
      native_session_id: str | None = None,
      started_at: str | None = None,
  ) -> Path:
    """One run dir with its metadata fields (the ones the capture reads) and raw NDJSON
    stream. The lines list takes no default: a mutable default would leak one call's
    corpus into the next."""
    run_dir = self.root / session / "data" / "runs" / run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    doc: dict = {"id": run_id, "session_id": session}
    if backend is not None:
      doc["backend"] = backend
    if model is not None:
      doc["model"] = model
    if native_session_id is not None:
      doc["native_session_id"] = native_session_id
    if started_at is not None:
      doc["started_at"] = started_at
    (run_dir / "metadata.json").write_text(json.dumps(doc))
    with (run_dir / "agent.raw.ndjson").open("w") as fh:
      for line in lines:
        fh.write(json.dumps(line) + "\n")
    return run_dir / "agent.raw.ndjson"


def _capture_runs(ledger: UsageLedger, sessions_dir: Path) -> int:
  """One Run capture round against *ledger*."""
  return tt.capture_runs(ledger, "host-a", sessions_dir)


def _cc_claude_init(sid: str) -> dict:
  """The claude CLI stream's init line, the session id its top level carries."""
  return {"type": "system", "subtype": "init", "session_id": sid, "model": "claude-opus-5-5"}


def _cc_claude_result(sid: str) -> dict:
  """The claude CLI stream's result line, usage in the Claude envelope keys."""
  return {
      "session_id": sid,
      "type": "result",
      "usage":
          {
              "input_tokens": 4,
              "cache_creation_input_tokens": 10,
              "cache_read_input_tokens": 100,
              "output_tokens": 7
          },
  }


def test_capture_runs_clc_run_is_native_with_the_cached_split(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A CLC run records natively on the model its metadata names, its cached reads split
  out of the result's one input field, and an unchanged run file is never re-parsed."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"))
  runs = Runs(tmp_path)
  runs.run(
      "s1",
      "r1", [{
          "type": "result",
          "usage": {
              "input_tokens": 100,
              "cached_tokens": 40,
              "output_tokens": 5
          }
      }],
      backend="charlie-code-glm53-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      started_at="2026-09-27T17:52:48.090388Z")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_runs(ledger, runs.root) == 1
    row = _ledger_rows(ledger)["charlie-bot", "GLM-5.3-Flash"]
    assert (row.calls, row.in_fresh, row.cache_write, row.cache_read, row.output) == (1, 60, 0, 40, 5)
    assert _capture_runs(ledger, runs.root) == 0  # the unchanged run file is skipped


def test_capture_runs_cc_claude_fallback_retires_when_the_transcript_is_captured(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A cc-claude run is a fallback row keyed on the session ids its metadata and its raw
  stream's init line carry, counted until the Claude transcript whose stem one of those
  ids names is captured; either id source retires the run."""
  _stub_registry(monkeypatch, _Option("claude-opus-5", "cc-claude", "anthropic/claude-opus-5-5"))
  runs = Runs(tmp_path)
  runs.run(
      "s1",
      "r1", [_cc_claude_result("cl-sess-1")],
      backend="claude-opus-5",
      model="claude-opus-5-5",
      native_session_id="cl-sess-1",
      started_at="2026-09-27T17:50:39.124791Z")
  runs.run(
      "s1",
      "r2", [_cc_claude_init("cl-sess-2"), _cc_claude_result("cl-sess-2")],
      backend="claude-opus-5",
      model=None,
      started_at="2026-09-28T12:46:23.854576Z")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_runs(ledger, runs.root) == 2  # no transcript yet: both fallback rows count
    row = _ledger_rows(ledger)["charlie-bot", "claude-opus-5-5"]
    assert (row.calls, row.fallback_calls, row.in_fresh, row.cache_write, row.cache_read,
            row.output) == (2, 2, 8, 20, 200, 14)

    claude = Claude(tmp_path)
    claude.write(claude.work, "cl-sess-1", [_claude_record("m1", NAME, "2026-09-27T17:50:39Z", _usage(4, 404))])
    assert _capture(claude, None, ledger)["Claude Code"] == 1
    row = _ledger_rows(ledger)["charlie-bot", "claude-opus-5-5"]
    assert (row.calls, row.fallback_calls) == (1, 1)  # r1 retired, the stream-id run r2 stays

    claude.write(claude.work, "cl-sess-2", [_claude_record("m2", NAME, "2026-09-28T12:46:23Z", _usage(4, 404))])
    assert _capture(claude, None, ledger)["Claude Code"] == 1
    assert ("charlie-bot", "claude-opus-5-5") not in _ledger_rows(ledger)  # r2 retired too


def test_capture_runs_codex_sums_turns_and_retires_on_the_captured_rollout(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A codex run sums its turn.completed lines into one fallback row keyed on the codex
  session id its metadata carries; capturing the rollout whose session id matches retires
  it, and a run with no turn.completed line contributes no record at all."""
  _stub_registry(monkeypatch, _Option("codex-gpt", "codex", "openai/gpt-5"))
  runs = Runs(tmp_path)
  runs.run(
      "s1",
      "r1", [
          {
              "type": "thread.started",
              "thread_id": "th-1"
          },
          {
              "type": "turn.completed",
              "usage": {
                  "input_tokens": 100,
                  "cached_input_tokens": 20,
                  "output_tokens": 5
              }
          },
          {
              "type": "turn.completed",
              "usage": {
                  "input_tokens": 50,
                  "cached_input_tokens": 10,
                  "output_tokens": 3
              }
          },
      ],
      backend="codex-gpt",
      model="openai/gpt-5",
      native_session_id="codex-sid-1",
      started_at="2026-09-27T17:52:48.090388Z")
  runs.run(
      "s1",
      "r2", [{
          "type": "thread.started",
          "thread_id": "th-2"
      }],
      backend="codex-gpt",
      model="openai/gpt-5",
      started_at="2026-09-27T17:50:24.123837Z")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_runs(ledger, runs.root) == 1  # r2 has no turn.completed line: no record
    row = _ledger_rows(ledger)["charlie-bot", "gpt-5"]
    assert (row.calls, row.fallback_calls, row.in_fresh, row.cache_read, row.output) == (1, 1, 120, 30, 8)

    codex = Codex(tmp_path)
    _codex_rollout(
        codex, "codex-sid-1", "gpt-5", {
            "input_tokens": 150,
            "cached_input_tokens": 30,
            "output_tokens": 8
        }, {"total_tokens": 128}, "2026-09-27T18:00:00Z")
    assert _capture(None, codex, ledger)["Codex"] == 1  # the rollout is captured native
    assert ("charlie-bot", "gpt-5") not in _ledger_rows(ledger)  # excluded: its session id has a native record


def test_capture_runs_writes_no_record_for_a_resultless_run_and_recaptures_nothing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A run killed before its result line contributes no record yet still records as
  captured (so it is never re-parsed), and an unchanged corpus's second capture writes
  nothing."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"))
  runs = Runs(tmp_path)
  runs.run(
      "s1",
      "r-lost", [{
          "type": "system",
          "subtype": "init",
          "session_id": "cl-sess-9"
      }],
      backend="charlie-code-glm53-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      started_at="2026-09-29T01:18:11.966557Z")
  runs.run(
      "s1",
      "r-done", [{
          "type": "result",
          "usage": {
              "input_tokens": 30,
              "cached_tokens": 10,
              "output_tokens": 2
          }
      }],
      backend="charlie-code-glm53-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      started_at="2026-09-29T01:20:00.000000Z")
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_runs(ledger, runs.root) == 1  # only the result-bearing run records
    row = _ledger_rows(ledger)["charlie-bot", "GLM-5.3-Flash"]
    assert (row.calls, row.in_fresh, row.output) == (1, 20, 2)
    assert set(ledger.captured_sigs("host-a")) == {
        str(runs.root / "s1" / "data" / "runs" / "r-lost" / "agent.raw.ndjson"),
        str(runs.root / "s1" / "data" / "runs" / "r-done" / "agent.raw.ndjson"),
    }  # the resultless file is marked captured too: never re-parsed
    assert _capture_runs(ledger, runs.root) == 0


# ---------------------------------------------------------------------------
# usage ledger capture (capture_opencode / capture_usage / capture_local)
# ---------------------------------------------------------------------------


class Opencode:
  """Synthetic opencode db: the message table schema and rows the real one carries."""

  def __init__(self, tmp_path: Path) -> None:
    self.db = tmp_path / "opencode.db"
    con = sqlite3.connect(self.db)
    try:
      con.execute(
          "create table message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,"
          " time_updated INTEGER, data TEXT)")
      con.commit()
    finally:
      con.close()

  def write(
      self,
      mid: str,
      updated: int,
      usage: dict,
      session: str = "sess-a",
      model: str = NAME,
      provider: str = "anthropic",
  ) -> None:
    """Insert or replace one assistant message row; *updated* is its time_updated."""
    data = {
        "role": "assistant",
        "modelID": model,
        "providerID": provider,
        "time": {
            "created": 1_700_000_000_000
        },
        "tokens":
            {
                "input": usage["input_tokens"],
                "output": usage["output_tokens"],
                "total": usage["input_tokens"] + usage["output_tokens"],
                "cache": {
                    "write": usage["cache_creation_input_tokens"],
                    "read": usage["cache_read_input_tokens"],
                },
            },
    }
    con = sqlite3.connect(self.db)
    try:
      con.execute(
          "insert or replace into message(id, session_id, time_created, time_updated, data)"
          " values (?, ?, ?, ?, ?)", (mid, session, 1_700_000_000_000, updated, json.dumps(data)))
      con.commit()
    finally:
      con.close()

  def delete(self, mid: str) -> None:
    con = sqlite3.connect(self.db)
    try:
      con.execute("delete from message where id = ?", (mid,))
      con.commit()
    finally:
      con.close()


def test_opencode_capture_parity_with_the_collect_rows(tmp_path: Path) -> None:
  """Every ledger row equals the matching collect row on source, model, the four token
  fields and calls: the capture projects the db through the same row function the collect
  reads, so a zero-token row drops from both and neither invents the other's rows."""
  oc = Opencode(tmp_path)
  oc.write("m1", 100, _usage(100, 30))
  oc.write("m2", 200, _usage(50, 5), model="gpt-5", provider="openai")
  oc.write("m3", 300, _usage(0, 0))  # zero tokens: no record in either path

  tally = _collect(None, None, oc.db)
  collect_rows = {(r.source, r.model): r for r in tally.rows if r.source == "opencode"}
  assert set(collect_rows) == {("opencode", NAME), ("opencode", "gpt-5")}
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert tt.capture_opencode(ledger, "host-a", oc.db) == 2
    ledger_rows = {(r.source, r.model): r for r in ledger.model_rows() if r.source == "opencode"}
  assert set(ledger_rows) == set(collect_rows)
  for key, lr in ledger_rows.items():
    cr = collect_rows[key]
    assert (lr.in_fresh, lr.cache_write, lr.cache_read, lr.output, lr.calls) == \
        (cr.in_fresh, cr.cache_write, cr.cache_read, cr.output, cr.calls)


def test_capture_opencode_rereads_updated_rows_and_keeps_deleted_rows_stored(tmp_path: Path) -> None:
  """A row moved to a higher time_updated is re-read and upserted over its stored record;
  rows deleted from the db afterwards leave the ledger rows untouched — the ledger contains
  no DELETE, so its rows outlive the db they were parsed from."""
  oc = Opencode(tmp_path)
  oc.write("m1", 100, _usage(100, 10))
  oc.write("m2", 200, _usage(200, 20))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert tt.capture_opencode(ledger, "host-a", oc.db) == 2

    oc.write("m2", 300, _usage(222, 22))  # same id, higher time_updated: a real update
    assert tt.capture_opencode(ledger, "host-a", oc.db) == 1  # only the moved row is re-read
    row = _ledger_row(ledger, "opencode", NAME)
    assert (row.in_fresh, row.output, row.calls) == (100 + 222, 10 + 22, 2)

    before = ledger.model_rows()
    oc.delete("m1")
    oc.delete("m2")
    assert tt.capture_opencode(ledger, "host-a", oc.db) == 0
    assert ledger.model_rows() == before


def test_capture_opencode_skips_an_unchanged_db_and_never_writes_to_it(tmp_path: Path) -> None:
  """A db whose probe signature the ledger already recorded writes nothing on the second
  call; the db opens read-only, so the capture leaves its bytes, mtime and directory
  untouched, and a missing db contributes nothing."""
  oc = Opencode(tmp_path)
  oc.write("m1", 100, _usage(100, 10))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert tt.capture_opencode(ledger, "host-a", tmp_path / "absent.sqlite") == 0
    assert tt.capture_opencode(ledger, "host-a", oc.db) == 1

    sig_before = (oc.db.stat().st_mtime_ns, oc.db.stat().st_size)
    files_before = sorted(p.name for p in tmp_path.iterdir())
    assert tt.capture_opencode(ledger, "host-a", oc.db) == 0
    assert (oc.db.stat().st_mtime_ns, oc.db.stat().st_size) == sig_before
    assert sorted(p.name for p in tmp_path.iterdir()) == files_before


def test_capture_usage_covers_every_source_and_zeroes_on_the_second_round(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """One call over Claude, Codex, opencode and a charlie-bot sessions dir writes each
  source's records and reports the per-source counts; a second call over the unchanged
  corpus writes all zeros."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.3-Flash"))
  claude = Claude(tmp_path)
  codex = Codex(tmp_path)
  oc = Opencode(tmp_path)
  cb = Charliebot(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1-id", NAME, "2024-01-01T00:00:00Z", _usage(100, 30))])
  _codex_rollout(
      codex, "codex-sid-1", "gpt-5", {
          "input_tokens": 60,
          "cached_input_tokens": 20,
          "output_tokens": 8
      }, {"total_tokens": 47}, "2024-01-03T00:00:00Z")
  oc.write("m1", 100, _usage(50, 5))
  cb.thread(
      "s1",
      "t1",
      backend="charlie-code-glm-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      session_ids=[],
      results=[("2026-09-11T20:00:00+00:00", _result_usage(200, 6))])
  cache = tmp_path / "cache.json"
  round_args = dict(
      host="host-a",
      claude_homes=claude.dirs,
      codex_homes=codex.homes,
      opencode_db=oc.db,
      sessions_dir=cb.root,
      cache_path=cache,
  )
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert tt.capture_usage(ledger, **round_args) == \
        {"Claude Code": 1, "Codex": 1, "opencode": 1, "charlie-bot": 1}
    assert tt.capture_usage(ledger, **round_args) == \
        {"Claude Code": 0, "Codex": 0, "opencode": 0, "charlie-bot": 0}
    # Sources the caller leaves unnamed are absent from the report, not zero-filled.
    assert tt.capture_usage(
        ledger, host="host-a", claude_homes={}, codex_homes={}, opencode_db=None, sessions_dir=None,
        cache_path=None) == {
            "Claude Code": 0,
            "Codex": 0
        }
