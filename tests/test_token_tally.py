"""Tests for the per-model token usage tally (src/core/token_tally.py).

Each test builds fixture log directories under tmp_path and points the collector at them directly,
so no test reads the real home directory. Every assertion checks a named mechanism rather than a
hard-coded total.
"""

from __future__ import annotations

import io
import json
from pathlib import Path
from typing import Any

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

  def raw_master(self, session: str, started: str, raw: str) -> Path:
    """One master-run capture written verbatim (for shapes other writers produce)."""
    run = self.root / session / "data" / "master_runs" / started
    run.mkdir(parents=True, exist_ok=True)
    (run / "agent.raw.ndjson").write_text(raw)
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
