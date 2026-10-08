"""Tests for the usage capture (src/features/usage/token_tally.py): per-source parsing into the
usage ledger's records.

Each test builds fixture logs under tmp_path and points HOME at it, so the registered sources' default
homes (``~/.claude``, ``~/.codex``, the opencode db) resolve there and no test reads the real home
directory. Every assertion checks a named mechanism rather than a hard-coded total.
"""

from __future__ import annotations

import json
import shutil
import sqlite3
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest
from conftest import codex_token_count_event

from src.backends.claude_code import usage_logs as claude_logs
from src.backends.codex import usage_logs as codex_logs
from src.features.usage import token_tally as tt
from src.features.usage.usage_ledger import UsageLedger
from src.infra import config, home, ndjson
from src.runtime.hooks import usage_sources

NAME = "claude-model"

# The source values the ledger stores; a change here is a ledger migration.
SOURCE_CHARLIE_BOT = "charlie-bot"
SOURCE_CHARLIE_CODE = "CLC"
SOURCE_CLAUDE_CODE = "Claude Code"
SOURCE_CODEX = "Codex"
SOURCE_OPENCODE = "opencode"


@pytest.fixture(autouse=True)
def _host(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """This host as the capture sees it: HOME under tmp_path, a config that names the second Claude
  login directory and no backend options, and tmp_path/sessions as the session tree."""
  monkeypatch.setenv("HOME", str(tmp_path))
  accounts = SimpleNamespace(claude=[SimpleNamespace(config_dir=str(tmp_path / ".claude-ext-1"))])
  cfg = SimpleNamespace(accounts=accounts, backends=SimpleNamespace(options=[]), sessions_dir=tmp_path / "sessions")
  monkeypatch.setattr(config, "get_config", lambda: cfg)


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


def _codex_count_solo(ts: str) -> dict:
  """One token_count event whose last usage equals its total (60 in, 20 cached, 7 out):
  the one-call rollout shape whose derived 40/20/7 row values the tests assert."""
  solo = {"input_tokens": 60, "cached_input_tokens": 20, "output_tokens": 7}
  return _codex_count(solo, solo, ts)


def _write_rollout(codex: Codex, sid: str, lines: list[dict]) -> Path:
  """One rollout named for *sid* — the file-name shape the real homes carry — with *lines*
  its whole record list, so fork trees and appended tails are written to order."""
  flow = codex.home / "sessions" / f"rollout-{sid}"
  flow.mkdir(parents=True, exist_ok=True)
  path = flow / f"rollout-{sid}.jsonl"
  with path.open("w") as fh:
    for line in lines:
      fh.write(json.dumps(line) + "\n")
  return path


def test_capture_usage_rows_are_absolutely_correct(tmp_path: Path) -> None:
  _claude_codex_corpus(tmp_path)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    written = _capture(ledger)
    assert (written["Claude Code"], written["Codex"]) == (3, 1)  # every parsed file's records, replays included

    cl = _ledger_row(ledger, "Claude Code", NAME)
    assert cl.calls == 2  # one original + one subagent; the replay is deduped
    assert cl.in_fresh == 100 + 50
    assert cl.cache_write == 20 + 10
    assert cl.cache_read == 40
    assert cl.output == 30 + 5
    assert cl.total == cl.in_fresh + cl.cache_write + cl.cache_read + cl.output
    assert (cl.first, cl.last) == ("2024-01-01", "2024-01-02")  # the subagent's response is the latest
    assert sum(a.total for a in cl.accounts) == cl.total

    cx = _ledger_row(ledger, "Codex", "codex-some")
    assert cx.in_fresh == 40  # 60 input - 20 cached
    assert cx.cache_read == 20
    assert cx.output == 7
    assert (cx.first, cx.last) == ("2024-01-03", "2024-01-03")
    assert cx.total == 40 + 20 + 7


def test_appends_are_visible(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(ledger)
    before = _ledger_row(ledger, "Claude Code", NAME)

    # A later session file records the same model: its total rises by exactly those tokens.
    claude.write(claude.work, "sess2", [_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(1000, 2))])
    _capture(ledger)
    after = _ledger_row(ledger, "Claude Code", NAME)
  assert after.total == before.total + 1002
  assert after.calls == before.calls + 1


def test_replays_are_not_double_counted(tmp_path: Path) -> None:
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(100, 10))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(ledger)
    before = _ledger_row(ledger, "Claude Code", NAME)

    # Copy the session file verbatim to a new session id (resume/fork behaviour).
    src = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
    dst = claude.work / "projects" / "rel" / "sess2"
    dst.mkdir(parents=True, exist_ok=True)
    (dst / "sess2.jsonl").write_text(src.read_text())

    _capture(ledger)
    after = _ledger_row(ledger, "Claude Code", NAME)
  assert after.total == before.total
  assert after.calls == before.calls


def _claude_rig(tmp_path: Path) -> Claude:
  """A Claude home carrying one m1 record in sess1 (usage 10/5): the state most tests start from."""
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  return claude


def test_parse_marker_lines_across_chunks_around_a_giant_line(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A no-marker line spanning many chunks parses once, never: the marker lines around it come out
  in file order and the trailing fragment without its newline waits (the gigabyte raw-log shape
  whose per-round re-concat paid O(line^2 / chunk))."""
  monkeypatch.setattr(ndjson, "_MARKER_CHUNK", 64)
  lines = [
      b'{"type": "context", "model": "clc-x"}',
      b'{"pad": "' + b"q" * 500 + b'"}',  # no marker, spans ~8 chunks
      b'{"type": "result", "usage": {"input_tokens": 3, "output_tokens": 4}}',
      b'{"pad2": "' + b"r" * 130 + b'"}',  # no marker, spans 2-3 chunks
      b'{"type": "result", "usage": {"input_tokens": 5, "output_tokens": 6}}',
  ]
  path = tmp_path / "giant.ndjson"
  path.write_bytes(b"\n".join(lines) + b"\n" + b'{"trailing": "fragment"}')
  objects = ndjson.parse_marker_lines(path, (b'"type": "context"', b'"type": "result"'))
  assert [(o.get("type"), o.get("usage", {}).get("input_tokens")) for o in objects] == [
      ("context", None), ("result", 3), ("result", 5)
  ]


def test_a_rewritten_file_reads_whole(tmp_path: Path) -> None:
  """A file whose signature moved reads whole — a replaced prefix or a shrink included — so its
  records restate its current content. The ledger keeps earlier records (no delete), so the row
  totals accumulate across the rewrites."""
  claude = _claude_rig(tmp_path)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(ledger)
    claude.write(  # whole-file rewrite: early content replaced, last line preserved
        claude.work, "sess1",
        [_claude_record("m9", NAME, "2024-01-03T00:00:00Z", _usage(7, 7)),
         _claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
    assert _capture(ledger)["Claude Code"] == 2  # both records, read whole
    row = _ledger_row(ledger, "Claude Code", NAME)
    assert (row.calls, row.total) == (2, 29)

    claude.write(claude.work, "sess1", [_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(3, 3))])
    assert _capture(ledger)["Claude Code"] == 1
    row = _ledger_row(ledger, "Claude Code", NAME)
    assert (row.calls, row.total) == (3, 35)  # m2 joins the records the ledger keeps


# ---------------------------------------------------------------------------
# charlie-bot source (thread event logs + master raw captures)
# ---------------------------------------------------------------------------


class _Option:
  """One config.yaml backend option's shape the capture reads (id / type / model)."""

  def __init__(self, option_id: str, option_type: str, model: str | None = None) -> None:
    self.id, self.type, self.model = option_id, option_type, model


def test_backend_page_source_attributes_by_type_prefix_and_master_account() -> None:
  """The page attribution reads a registered id's config type first (the type wins over
  the id's prefix), an off-config id's type prefix next, and the master-run account as
  CLC; a backend type with no collection rule raises, naming the id, instead of landing
  under a wrong CLI."""
  registry = {
      "charlie-code-glm": _Option("charlie-code-glm", "charlie-code", "openai/zai-org/GLM-5.3-Flash"),
      "codex-gpt": _Option("codex-gpt", "codex", "openai/gpt-5"),
      "claude-opus": _Option("claude-opus", "cc-claude", "anthropic/claude-opus-5-5"),
      "kimi-k3": _Option("kimi-k3", "cc-kimi", "moonshotai/Kimi-K3"),
      "glm-air": _Option("glm-air", "cc-openai-compatible", "openai/GLM-Air"),
      "oc-qwen": _Option("oc-qwen", "opencode", "qwen/qwen3"),
      # A registered id whose type disagrees with its prefix: the type wins.
      "claude-mislabeled": _Option("claude-mislabeled", "codex", "openai/gpt-5"),
  }
  assert tt.backend_page_source("charlie-code-glm", registry) == SOURCE_CHARLIE_CODE
  assert tt.backend_page_source("codex-gpt", registry) == SOURCE_CODEX
  assert tt.backend_page_source("claude-opus", registry) == SOURCE_CLAUDE_CODE
  assert tt.backend_page_source("kimi-k3", registry) == SOURCE_CLAUDE_CODE
  assert tt.backend_page_source("glm-air", registry) == SOURCE_CLAUDE_CODE
  assert tt.backend_page_source("oc-qwen", registry) == SOURCE_OPENCODE
  assert tt.backend_page_source("claude-mislabeled", registry) == SOURCE_CODEX
  assert tt.backend_page_source("charlie-code-retired", {}) == SOURCE_CHARLIE_CODE
  assert tt.backend_page_source("codex-retired", {}) == SOURCE_CODEX
  assert tt.backend_page_source("claude-retired", {}) == SOURCE_CLAUDE_CODE
  assert tt.backend_page_source("opencode-retired", {}) == SOURCE_OPENCODE
  assert tt.backend_page_source("clc-master", {}) == SOURCE_CHARLIE_CODE
  with pytest.raises(ValueError, match="ag-1"):
    tt.backend_page_source("ag-1", {"ag-1": _Option("ag-1", "antigravity", "model")})
  with pytest.raises(ValueError, match="gem-1"):
    tt.backend_page_source("gem-1", {})


def _stub_registry(monkeypatch: pytest.MonkeyPatch, *options: _Option) -> None:
  monkeypatch.setattr(tt, "backend_registry", lambda: {opt.id: opt for opt in options})


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

  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root) == 3
    recorded = _ledger_row(ledger, "charlie-bot", "GLM-5.3-Flash")
    assert recorded.calls == 1 and recorded.total == 105
    assert [a.name for a in recorded.accounts] == ["charlie-code-glm-flash"]
    assert _ledger_row(ledger, "charlie-bot", "GLM-5.4-Flash").total == 205
    retired = _ledger_row(ledger, "charlie-bot", "kimi-k3")
    assert retired.total == 305
    assert [a.name for a in retired.accounts] == ["charlie-code-kimi-k3"]


# ---------------------------------------------------------------------------
# usage ledger capture (capture_usage)
# ---------------------------------------------------------------------------


def _claude_codex_corpus(tmp_path: Path) -> tuple[Claude, Codex]:
  """The Claude and Codex corpus: an original response, its verbatim replay in a
  second config dir, a subagent response, and one Codex rollout."""
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
  codex.write(
      "rollout", [
          _codex_meta(session_id="rolloutroot1"),
          _codex_turn("codex-some"),
          _codex_count_solo("2024-01-03T00:00:00Z"),
      ])
  return claude, codex


def _capture(ledger: UsageLedger) -> dict[str, int]:
  """One capture round against *ledger* over this host's logs and session tree, under the "host-a" label."""
  return tt.capture_usage(ledger, host="host-a", sessions_dir=config.get_config().sessions_dir)


def _ledger_row(ledger: UsageLedger, source: str, model: str):
  return next(r for r in ledger.model_rows() if r.source == source and r.model == model)


def test_captured_rows_survive_deleting_the_source_files(tmp_path: Path) -> None:
  """Capturing again over deleted sources leaves every ledger row equal field by field:
  the ledger contains no delete, so its rows outlive the logs they were parsed from."""
  claude, codex = _claude_codex_corpus(tmp_path)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(ledger)
    before = ledger.model_rows()
    shutil.rmtree(claude.work)
    shutil.rmtree(claude.ext)
    shutil.rmtree(codex.home)
    _capture(ledger)
    assert ledger.model_rows() == before


def test_second_capture_without_changes_writes_nothing(tmp_path: Path) -> None:
  """A file the ledger already holds at the same signature is skipped: the second capture
  over an unchanged corpus writes zero records."""
  _claude_codex_corpus(tmp_path)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    first = _capture(ledger)
    assert (first["Claude Code"], first["Codex"]) == (3, 1)  # every parsed file's records, replays included
    assert set(_capture(ledger).values()) == {0}


def test_appended_line_is_captured_and_second_dir_replay_counts_once(tmp_path: Path) -> None:
  """An appended line reaches the ledger on the next capture (the file rewrites whole: its
  captured records re-upsert beside the new one), and a message id replayed into a second
  config dir is captured yet still counted once."""
  claude = Claude(tmp_path)
  claude.write(claude.work, "sess1", [_claude_record("m1", NAME, "2024-01-01T00:00:00Z", _usage(10, 5))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(ledger)
    before = _ledger_row(ledger, "Claude Code", NAME)

    # Resume/fork behaviour: the session file copied verbatim into the second config dir.
    src = claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl"
    replay_dir = claude.ext / "projects" / "rel" / "sess1"
    replay_dir.mkdir(parents=True)
    (replay_dir / "sess1.jsonl").write_text(src.read_text())
    assert _capture(ledger)["Claude Code"] == 1  # the replay file is captured too
    replayed = _ledger_row(ledger, "Claude Code", NAME)
    assert replayed.calls == before.calls == 1  # ... yet its message id counts once

    with src.open("a") as fh:
      fh.write(json.dumps(_claude_record("m2", NAME, "2024-01-02T00:00:00Z", _usage(100, 2))) + "\n")
    written = _capture(ledger)
    after = _ledger_row(ledger, "Claude Code", NAME)

  assert written["Claude Code"] == 2  # m1 re-upserts beside the appended m2
  assert after.calls == 2 and after.output == 7
  assert (after.first, after.last) == ("2024-01-01", "2024-01-02")


# ---------------------------------------------------------------------------
# Codex records: one per API call, keyed on the event's cumulative totals
# ---------------------------------------------------------------------------


def _codex_rows(ledger: UsageLedger) -> list:
  """Every Codex-source usage row, ordered by record id: the ground the assertions read."""
  return ledger._conn.execute(
      "SELECT record_id, model, ts, in_fresh, cache_read, output FROM usage WHERE source = ? ORDER BY record_id",
      (SOURCE_CODEX,)).fetchall()


def test_forked_rollout_counts_the_copied_events_once(tmp_path: Path) -> None:
  """A forked rollout opens with its parent's events copied: they ride the parent's record
  ids, so five calls land as five records and each copied call keeps the parent's ts."""
  codex = Codex(tmp_path)
  root = "forktreeroot1"
  calls = [
      # (per-call last_token_usage, cumulative total_token_usage, ts) after each request.
      (
          {
              "input_tokens": 60,
              "cached_input_tokens": 20,
              "output_tokens": 7
          }, {
              "input_tokens": 60,
              "cached_input_tokens": 20,
              "output_tokens": 7
          }, "2026-09-10T00:00:01Z"),
      (
          {
              "input_tokens": 30,
              "cached_input_tokens": 0,
              "output_tokens": 5
          }, {
              "input_tokens": 90,
              "cached_input_tokens": 20,
              "output_tokens": 12
          }, "2026-09-10T00:00:02Z"),
      (
          {
              "input_tokens": 10,
              "cached_input_tokens": 4,
              "output_tokens": 1
          }, {
              "input_tokens": 100,
              "cached_input_tokens": 24,
              "output_tokens": 13
          }, "2026-09-10T00:00:03Z"),
  ]
  parent = [_codex_meta(session_id=root), _codex_turn("codex-m1")]
  parent += [_codex_count(last, total, ts) for last, total, ts in calls]
  _write_rollout(codex, "forkparent1", parent)

  # The fork copies the parent's first two events but stamps them at fork time, then adds
  # two calls of its own whose totals continue the tree's.
  fork = [_codex_meta(session_id=root), _codex_turn("codex-m1")]
  fork += [_codex_count(last, total, f"2026-09-10T00:09:0{i}Z") for i, (last, total, _ts) in enumerate(calls[:2], 1)]
  fork += [
      _codex_count(
          {
              "input_tokens": 26,
              "cached_input_tokens": 6,
              "output_tokens": 3
          }, {
              "input_tokens": 126,
              "cached_input_tokens": 30,
              "output_tokens": 16
          }, "2026-09-10T00:09:03Z"),
      _codex_count(
          {
              "input_tokens": 5,
              "cached_input_tokens": 0,
              "output_tokens": 2
          }, {
              "input_tokens": 131,
              "cached_input_tokens": 30,
              "output_tokens": 18
          }, "2026-09-10T00:09:04Z"),
  ]
  _write_rollout(codex, "forkchild1", fork)

  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(ledger)
    rows = _codex_rows(ledger)

  assert len(rows) == 5  # 3 parent calls + 2 fork-own calls; the 2 copies ride the parent's ids
  by_id = {row["record_id"]: row for row in rows}
  assert set(by_id) == {
      "codex-total:forktreeroot1:60:20:7",
      "codex-total:forktreeroot1:90:20:12",
      "codex-total:forktreeroot1:100:24:13",
      "codex-total:forktreeroot1:126:30:16",
      "codex-total:forktreeroot1:131:30:18",
  }
  assert by_id["codex-total:forktreeroot1:126:30:16"]["ts"] == "2026-09-10T00:09:03Z"
  own = by_id["codex-total:forktreeroot1:126:30:16"]
  assert (own["in_fresh"], own["cache_read"], own["output"]) == (20, 6, 3)
  # The copies carry the fork's stamps until the parent's write restores the call's own ts.
  assert by_id["codex-total:forktreeroot1:60:20:7"]["ts"] == "2026-09-10T00:00:01Z"
  assert by_id["codex-total:forktreeroot1:90:20:12"]["ts"] == "2026-09-10T00:00:02Z"


@pytest.mark.parametrize("real_first", [True, False], ids=["real-rollout-first", "fork-rollout-first"])
def test_reemitted_events_add_no_record_and_overwrite_nothing(tmp_path: Path, real_first: bool) -> None:
  """A verbatim re-emit, a zeroed re-emit and an info:null event change nothing: whichever
  rollout records first, the real call stays the only record with its own values."""
  codex = Codex(tmp_path)
  root = "reemitroot1"
  last = {"input_tokens": 60, "cached_input_tokens": 20, "output_tokens": 7}
  total = {"input_tokens": 60, "cached_input_tokens": 20, "output_tokens": 7}
  real = [_codex_meta(session_id=root), _codex_turn("codex-m1"), _codex_count(last, total, "2026-09-10T00:00:00Z")]
  # The fork re-writes the parent's event three ways: verbatim, with the per-request usage
  # zeroed, and without any info at all.
  fork = [
      _codex_meta(session_id=root),
      _codex_turn("codex-m1"),
      _codex_count(last, total, "2026-09-10T00:01:00Z"),
      _codex_count({
          "input_tokens": 0,
          "cached_input_tokens": 0,
          "output_tokens": 0
      }, total, "2026-09-10T00:02:00Z"),
      codex_token_count_event("2026-09-10T00:03:00Z", info=None),
  ]
  first_sid, first_lines, second_sid, second_lines = (
      ("reemitreal1", real, "reemitfork1", fork) if real_first else ("reemitfork1", fork, "reemitreal1", real))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _write_rollout(codex, first_sid, first_lines)
    _capture(ledger)
    _write_rollout(codex, second_sid, second_lines)
    _capture(ledger)
    rows = _codex_rows(ledger)

  assert len(rows) == 1
  assert rows[0]["record_id"] == "codex-total:reemitroot1:60:20:7"
  assert (rows[0]["in_fresh"], rows[0]["cache_read"], rows[0]["output"]) == (40, 20, 7)
  assert rows[0]["ts"] == "2026-09-10T00:00:00Z"  # the call's own ts, whichever file recorded first


def test_codex_p2_signature_rereads_the_unchanged_rollout_once(tmp_path: Path) -> None:
  """A rollout stored under the previous release's ``p2:`` signature never matches the bare stat
  pair, so the unchanged file reads again once and then skips on the bare pair."""
  codex = Codex(tmp_path)
  path = _write_rollout(
      codex, "sigthread1", [
          _codex_meta(session_id="sigthread1"),
          _codex_turn("codex-m1"),
          _codex_count_solo("2026-09-10T00:00:00Z"),
      ])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    st = path.stat()
    ledger.record_file("host-a", str(path), f"p2:{st.st_mtime_ns}:{st.st_size}", [])
    assert _capture(ledger)["Codex"] == 1  # the p2: signature never matches
    assert _capture(ledger)["Codex"] == 0  # the stored bare pair does


def test_rollout_without_a_session_id_fails_the_capture(tmp_path: Path) -> None:
  """A token_count-bearing rollout whose session_meta carries no session_id stops the
  capture with an error naming the file."""
  codex = Codex(tmp_path)
  _write_rollout(
      codex, "norootthread1", [
          _codex_meta(),
          _codex_turn("codex-m1"),
          _codex_count_solo("2026-09-10T00:00:00Z"),
      ])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger, pytest.raises(ValueError, match="norootthread1"):
    _capture(ledger)


# ---------------------------------------------------------------------------
# usage ledger capture (capture_charliebot)
# ---------------------------------------------------------------------------


def _capture_charliebot(ledger: UsageLedger, sessions_dir: Path) -> int:
  """One charlie-bot capture round against *ledger*."""
  return tt.capture_charliebot(ledger, "host-a", sessions_dir, ledger.captured_sigs("host-a"))


def _ledger_rows(ledger: UsageLedger) -> dict:
  return {(r.source, r.model): r for r in ledger.model_rows()}


def _codex_rollout(codex: Codex, sid: str, model: str, last: dict, total: dict, ts: str) -> Path:
  """One Codex rollout named for its session id — the name the real homes carry and the
  capture's session registration parses. Its own session_meta names the root, so a
  single-thread rollout is its own fork tree."""
  flow = codex.home / "sessions" / f"rollout-{sid}"
  flow.mkdir(parents=True, exist_ok=True)
  with (flow / f"rollout-{sid}.jsonl").open("w") as fh:
    for line in (_codex_meta(session_id=sid), _codex_turn(model), _codex_count(last, total, ts)):
      fh.write(json.dumps(line) + "\n")
  return flow / f"rollout-{sid}.jsonl"


def test_charliebot_capture_rows(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """CLC threads plus a CLC master capture give the ledger one row per model — the recorded
  model naming the first thread's row, the config model and the master's context model
  merging into the second — and an unchanged corpus's second capture writes nothing."""
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
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root) == 3
    assert _capture_charliebot(ledger, cb.root) == 0  # the unchanged corpus is skipped
    rows = {(r.source, r.model): r for r in ledger.model_rows() if r.source == "charlie-bot"}

  assert set(rows) == {("charlie-bot", "GLM-5.3-Flash"), ("charlie-bot", "GLM-5.4-Flash")}
  recorded = rows["charlie-bot", "GLM-5.3-Flash"]
  # A CLC thread's usage logs no cache fields: the whole input lands in in_unsplit.
  assert (
      recorded.calls, recorded.in_fresh, recorded.cache_write, recorded.cache_read, recorded.in_unsplit,
      recorded.output) == (1, 0, 0, 0, 100, 5)
  merged = rows["charlie-bot", "GLM-5.4-Flash"]
  # t2's input is unsplit like t1's; the master's carries no cached reads.
  assert (merged.calls, merged.in_fresh, merged.in_unsplit, merged.output) == (2, 300, 200, 6 + 7)


def test_charliebot_codex_thread_counts_its_cached_reads_once(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A codex-type thread result's input_tokens already includes its cached reads, so
  in_fresh keeps only the uncached part and the reads land in cache_read — counted once,
  not once per column."""
  _stub_registry(monkeypatch, _Option("codex-gpt", "codex", "openai/gpt-5"))
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="codex-gpt",
      model=None,
      session_ids=["sid-1"],
      results=[("2026-09-11T20:00:00+00:00", _result_usage(906453, 4104, cache_read=846080))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root) == 1
    row = _ledger_rows(ledger)["charlie-bot", "gpt-5"]
    assert (row.calls, row.in_fresh, row.cache_write, row.cache_read, row.output) == (1, 60373, 0, 846080, 4104)
    assert row.total == 906453 + 4104  # the envelope's input counted once, cached part included


def test_charliebot_clc_thread_lands_whole_in_unsplit(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A CLC thread result's usage logs no cache fields, so the hit/miss split is unknowable:
  the whole input counts in in_unsplit instead of reading as a fresh miss in in_fresh."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"))
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="charlie-code-glm-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      session_ids=[],
      results=[("2026-09-11T20:00:00+00:00", {
          "input_tokens": 4677324,
          "output_tokens": 39171
      })])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root) == 1
    row = _ledger_rows(ledger)["charlie-bot", "GLM-5.3-Flash"]
    assert (row.calls, row.in_fresh, row.cache_write, row.cache_read, row.in_unsplit, row.output) == \
        (1, 0, 0, 0, 4677324, 39171)
    assert row.total == 4677324 + 39171


def test_charliebot_cc_claude_thread_keeps_the_claude_split(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A cc-claude thread result's usage is in the Claude envelope keys, which carry the
  split themselves: the counts stay the envelope's own, whatever the codex and CLC
  envelopes needed."""
  _stub_registry(monkeypatch, _Option("claude-opus-5", "cc-claude", "anthropic/claude-opus-5-5"))
  cb = Charliebot(tmp_path)
  cb.thread(
      "s1",
      "t1",
      backend="claude-opus-5",
      model="claude-opus-5-5",
      session_ids=["cl-sess"],
      results=[
          (
              "2026-09-11T20:00:00+00:00", {
                  "input_tokens": 100,
                  "cache_creation_input_tokens": 20,
                  "cache_read_input_tokens": 40,
                  "output_tokens": 5,
              })
      ])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root) == 1
    row = _ledger_rows(ledger)["charlie-bot", "claude-opus-5-5"]
    assert (row.calls, row.in_fresh, row.cache_write, row.cache_read, row.in_unsplit, row.output) == \
        (1, 100, 20, 40, 0, 5)


def test_charliebot_master_counts_its_cached_reads_once(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A master capture's trailing result carries the run's usage with the cached reads
  inside the one input field; they land in cache_read, not in in_fresh beside them."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"))
  cb = Charliebot(tmp_path)
  cb.master(
      "s1", "20260913T000000Z", [
          {
              "type": "context",
              "model": "openai/zai-org/GLM-5.4-Flash"
          },
          {
              "type": "result",
              "usage": {
                  "input_tokens": 1293011,
                  "cached_tokens": 1283200,
                  "output_tokens": 8712
              }
          },
      ])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture_charliebot(ledger, cb.root) == 1
    row = _ledger_rows(ledger)["charlie-bot", "GLM-5.4-Flash"]
    assert (row.calls, row.in_fresh, row.cache_write, row.cache_read, row.in_unsplit, row.output) == \
        (1, 9811, 0, 1283200, 0, 8712)
    assert row.total == 1293011 + 8712


def test_charliebot_codex_thread_is_skipped_while_a_rollout_of_it_exists(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """The Codex rule: a codex-type thread any of whose session ids names a rollout still on disk
  contributes nothing — the rollout's own native rows are the usage, and a partly pruned
  multi-session thread must not count twice. A thread none of whose ids has a rollout counts as
  a fallback row. Deleting the rollout afterwards changes no row."""
  _stub_registry(monkeypatch, _Option("codex-gpt", "codex", "openai/gpt-5"))
  sid = "rolloutsid1"
  codex = Codex(tmp_path)
  _codex_rollout(
      codex, sid, "codex-gpt5", {
          "input_tokens": 60,
          "cached_input_tokens": 20,
          "output_tokens": 7
      }, {
          "input_tokens": 60,
          "cached_input_tokens": 20,
          "output_tokens": 7
      }, "2026-09-10T00:00:00Z")
  cb = Charliebot(tmp_path)
  for tid, ids in (("t1", [sid]), ("t2", [sid, "pruned-sid"]), ("t3", ["orphan-sid"])):
    cb.thread(
        "s1",
        tid,
        backend="codex-gpt",
        model=None,
        session_ids=ids,
        results=[("2026-09-11T20:00:00+00:00", _result_usage(100, 5))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    written = _capture(ledger)
    assert (written["Codex"], written["charlie-bot"]) == (1, 1)  # only t3's thread is stored
    rows = _ledger_rows(ledger)
    assert rows["Codex", "codex-gpt5"].calls == 1
    assert (rows["charlie-bot", "gpt-5"].calls, rows["charlie-bot", "gpt-5"].fallback_calls) == (1, 1)
    assert len(ledger.captured_sigs("host-a")) == 4  # the skipped threads are recorded too: never read again

    shutil.rmtree(codex.home)
    assert set(_capture(ledger).values()) == {0}  # the native rows survive the deletion
    assert _ledger_rows(ledger) == rows


def test_charliebot_fallback_rows_count_until_the_cli_log_is_captured(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A codex-type thread with no rollout anywhere and a claude-type thread with no Claude
  transcript are counted as fallback rows; capturing the Claude transcript whose stem the
  thread's session id names retires that row, while the codex thread's row stays."""
  _stub_registry(
      monkeypatch, _Option("codex-gpt", "codex", "openai/gpt-5"),
      _Option("claude-sonnet", "cc-claude", "anthropic/claude-sonnet-4"))
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
    assert _capture(ledger)["Claude Code"] == 1
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


def test_charliebot_capture_reparses_a_file_the_bare_signature_recorded(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A thread file the older parser recorded under the bare stat pair re-parses once and
  rewrites its rows onto the same record ids; a file whose p2 signature the ledger holds
  is skipped."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"))
  cb = Charliebot(tmp_path)
  path = cb.thread(
      "s1",
      "t1",
      backend="charlie-code-glm-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      session_ids=[],
      results=[("2026-09-11T20:00:00+00:00", {
          "input_tokens": 100,
          "output_tokens": 5
      })])
  st = path.stat()
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    # What the older parser wrote: the whole input in in_fresh, under the bare stat pair.
    stale = usage_sources.UsageRecord(
        record_id="thread:s1/t1/0",
        kind=usage_sources.RecordKind.NATIVE,
        source=SOURCE_CHARLIE_BOT,
        model="GLM-5.3-Flash",
        account="charlie-code-glm-flash",
        ts="2026-09-11T20:00:00+00:00",
        in_fresh=100,
        cache_write=0,
        cache_read=0,
        output=5)
    assert ledger.record_file("host-a", str(path), f"{st.st_mtime_ns}:{st.st_size}", [stale]) == 1
    assert _capture_charliebot(ledger, cb.root) == 1  # the bare stat pair is no current signature
    row = _ledger_rows(ledger)["charlie-bot", "GLM-5.3-Flash"]
    assert (row.calls, row.in_fresh, row.in_unsplit, row.output) == (1, 0, 100, 5)  # same id, corrected values
    assert ledger.captured_sigs("host-a")[str(path)] == f"p2:{st.st_mtime_ns}:{st.st_size}"
    assert _capture_charliebot(ledger, cb.root) == 0  # the matching p2 signature serves


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
  return tt.capture_runs(ledger, "host-a", sessions_dir, ledger.captured_sigs("host-a"))


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
    assert _capture(ledger)["Claude Code"] == 1
    row = _ledger_rows(ledger)["charlie-bot", "claude-opus-5-5"]
    assert (row.calls, row.fallback_calls) == (1, 1)  # r1 retired, the stream-id run r2 stays

    claude.write(claude.work, "cl-sess-2", [_claude_record("m2", NAME, "2026-09-28T12:46:23Z", _usage(4, 404))])
    assert _capture(ledger)["Claude Code"] == 1
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
        }, {
            "input_tokens": 150,
            "cached_input_tokens": 30,
            "output_tokens": 8
        }, "2026-09-27T18:00:00Z")
    assert _capture(ledger)["Codex"] == 1  # the rollout is captured native
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


def test_capture_runs_second_capture_skips_by_stat_and_recaptures_an_append(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An unchanged run file's second capture decides on its stat pair alone —
  parse_marker_lines is never called — and an appended result line moves the stat, so the
  capture re-parses and upserts the run's record on the new usage."""
  _stub_registry(monkeypatch, _Option("charlie-code-glm53-flash", "charlie-code", "openai/zai-org/GLM-5.4-Flash"))
  runs = Runs(tmp_path)
  log = runs.run(
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

    def _refuse_parse(path: str, markers: tuple[bytes, ...]) -> tuple:
      raise AssertionError(f"unchanged corpus re-parsed a run log: {path}")

    real_parse = ndjson.parse_marker_lines
    monkeypatch.setattr(ndjson, "parse_marker_lines", _refuse_parse)
    assert _capture_runs(ledger, runs.root) == 0  # the stat pair alone decides the skip
    monkeypatch.setattr(ndjson, "parse_marker_lines", real_parse)

    with log.open("a") as fh:
      fh.write(
          json.dumps({
              "type": "result",
              "usage": {
                  "input_tokens": 200,
                  "cached_tokens": 10,
                  "output_tokens": 9
              }
          }) + "\n")
    assert _capture_runs(ledger, runs.root) == 1  # the append moved the stat: re-parsed
    row = _ledger_rows(ledger)["charlie-bot", "GLM-5.3-Flash"]
    assert (row.calls, row.in_fresh, row.cache_read, row.output) == (1, 190, 10, 9)


# ---------------------------------------------------------------------------
# usage ledger capture (opencode, every source, the capture's bookkeeping)
# ---------------------------------------------------------------------------


class Opencode:
  """Synthetic opencode db: the message table schema and rows the real one carries."""

  def __init__(self, tmp_path: Path) -> None:
    self.db = tmp_path / home.default_opencode_db().relative_to(tmp_path)
    self.db.parent.mkdir(parents=True)
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


def test_opencode_capture_rows(tmp_path: Path) -> None:
  """Every contributing message row becomes one ledger record; a zero-token row contributes
  nothing, so the db's non-usage rows invent no ledger rows; a host without the db contributes
  nothing."""
  with UsageLedger(tmp_path / "absent.sqlite3") as ledger:
    assert _capture(ledger)["opencode"] == 0
  oc = Opencode(tmp_path)
  oc.write("m1", 100, _usage(100, 30))
  oc.write("m2", 200, _usage(50, 5), model="gpt-5", provider="openai")
  oc.write("m3", 300, _usage(0, 0))  # zero tokens: no record

  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture(ledger)["opencode"] == 2
    rows = {(r.source, r.model): r for r in ledger.model_rows() if r.source == "opencode"}
  assert set(rows) == {("opencode", NAME), ("opencode", "gpt-5")}
  anthropic = rows["opencode", NAME]
  assert (anthropic.calls, anthropic.in_fresh, anthropic.output) == (1, 100, 30)
  openai_row = rows["opencode", "gpt-5"]
  assert (openai_row.calls, openai_row.in_fresh, openai_row.output) == (1, 50, 5)


def test_capture_opencode_rereads_updated_rows_and_keeps_deleted_rows_stored(tmp_path: Path) -> None:
  """A row moved to a higher time_updated is re-read and upserted over its stored record;
  rows deleted from the db afterwards leave the ledger rows untouched — the ledger contains
  no DELETE, so its rows outlive the db they were parsed from."""
  oc = Opencode(tmp_path)
  oc.write("m1", 100, _usage(100, 10))
  oc.write("m2", 200, _usage(200, 20))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture(ledger)["opencode"] == 2

    oc.write("m2", 300, _usage(222, 22))  # same id, higher time_updated: a real update
    assert _capture(ledger)["opencode"] == 1  # only the moved row is re-read
    row = _ledger_row(ledger, "opencode", NAME)
    assert (row.in_fresh, row.output, row.calls) == (100 + 222, 10 + 22, 2)

    before = ledger.model_rows()
    oc.delete("m1")
    oc.delete("m2")
    assert _capture(ledger)["opencode"] == 0
    assert ledger.model_rows() == before


def test_capture_opencode_skips_an_unchanged_db_and_never_writes_to_it(tmp_path: Path) -> None:
  """A db whose files sit still since the stored signature writes nothing on the next capture;
  the db opens read-only, so the capture leaves its bytes, mtime and directory untouched."""
  oc = Opencode(tmp_path)
  oc.write("m1", 100, _usage(100, 10))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture(ledger)["opencode"] == 1

    sig_before = (oc.db.stat().st_mtime_ns, oc.db.stat().st_size)
    files_before = sorted(p.name for p in oc.db.parent.iterdir())
    assert _capture(ledger)["opencode"] == 0
    assert (oc.db.stat().st_mtime_ns, oc.db.stat().st_size) == sig_before
    assert sorted(p.name for p in oc.db.parent.iterdir()) == files_before


def test_capture_usage_covers_every_source_and_zeroes_on_the_second_round(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """One call over Claude, Codex, opencode and a charlie-bot sessions dir writes each source's
  records and reports the per-source counts; a second call over the unchanged corpus writes all
  zeros. CLC has no log of its own, so only the charlie-bot label reports its records."""
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
      }, {
          "input_tokens": 60,
          "cached_input_tokens": 20,
          "output_tokens": 8
      }, "2024-01-03T00:00:00Z")
  oc.write("m1", 100, _usage(50, 5))
  cb.thread(
      "s1",
      "t1",
      backend="charlie-code-glm-flash",
      model="openai/zai-org/GLM-5.3-Flash",
      session_ids=[],
      results=[("2026-09-11T20:00:00+00:00", _result_usage(200, 6))])
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture(ledger) == {"Claude Code": 1, "Codex": 1, "opencode": 1, "charlie-bot": 1}
    assert _capture(ledger) == {"Claude Code": 0, "Codex": 0, "opencode": 0, "charlie-bot": 0}


# ---------------------------------------------------------------------------
# the capture's signature-first contract and its bookkeeping
# ---------------------------------------------------------------------------


def test_a_file_at_its_stored_signature_is_never_read(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A Claude or Codex file whose stat pair equals the stored signature skips its source's
  ``read()`` altogether, so an unchanged corpus costs one stat per file; a file that moved reads again."""
  claude, _codex = _claude_codex_corpus(tmp_path)
  real_claude_read, real_codex_read = claude_logs.read, codex_logs.read

  def refuse(path: Path, account: str, previous: str | None) -> tuple:
    raise AssertionError(f"read() of an unchanged file: {path}")

  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    _capture(ledger)
    monkeypatch.setattr(claude_logs, "read", refuse)
    monkeypatch.setattr(codex_logs, "read", refuse)
    assert set(_capture(ledger).values()) == {0}  # every file skipped before its read

    monkeypatch.setattr(claude_logs, "read", real_claude_read)
    monkeypatch.setattr(codex_logs, "read", real_codex_read)
    with (claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl").open("a") as fh:
      fh.write(json.dumps(_claude_record("m2", NAME, "2024-01-04T00:00:00Z", _usage(1, 1))) + "\n")
    assert _capture(ledger)["Claude Code"] == 2  # the moved file reads again; only it does
    assert _capture(ledger)["Codex"] == 0


def test_old_format_opencode_signature_forces_one_reread(tmp_path: Path) -> None:
  """The opencode db's signature is five numeric parts; a stored signature in any other format
  reads the whole table once, and the capture stores the current format, which then skips."""
  oc = Opencode(tmp_path)
  oc.write("m1", 100, _usage(100, 10))
  oc.write("m2", 200, _usage(200, 20))
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture(ledger)["opencode"] == 2
    ledger.record_file("host-a", str(oc.db), "legacy-gate-signature", [])
    assert _capture(ledger)["opencode"] == 2  # the whole table, not just the rows past the stored max
    assert len(ledger.captured_sigs("host-a")[str(oc.db)].split(":")) == 5
    assert _capture(ledger)["opencode"] == 0
    assert _ledger_row(ledger, "opencode", NAME).calls == 2  # the reread upserted onto the same ids


def test_a_finished_capture_stamps_last_capture_at_and_a_failed_one_does_not(tmp_path: Path) -> None:
  """``last_capture_at`` is the page's "as of": a capture that returns stamps its host, and a
  capture that raises leaves the stamp alone."""
  codex = Codex(tmp_path)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert ledger.last_capture_at("host-a") is None
    _capture(ledger)
    stamped = ledger.last_capture_at("host-a")
    assert stamped is not None
    assert ledger.last_capture_at("host-b") is None

    _write_rollout(codex, "norootthread1", [_codex_meta(), _codex_turn("codex-m1"), _codex_count_solo("ts")])
    with pytest.raises(ValueError, match="norootthread1"):
      _capture(ledger)
    assert ledger.last_capture_at("host-a") == stamped


def test_capture_without_a_registered_source_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An entry point that skipped ``registrations.register_all()`` fails the capture instead of
  capturing only the charlie-bot logs and stamping the ledger as current."""
  monkeypatch.setattr(usage_sources, "_sources", {})
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger, pytest.raises(RuntimeError, match="register_all"):
    _capture(ledger)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert ledger.last_capture_at("host-a") is None


def test_capture_runs_opencode_run_is_a_fallback_through_the_claude_envelope_keys(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """Every backend whose source is neither run-logs-only nor under the Codex rule reads its run's
  trailing result through the Claude envelope keys: an opencode-type run is a fallback row that
  retires once the opencode session its metadata names is captured."""
  _stub_registry(monkeypatch, _Option("oc-qwen", "opencode", "qwen/qwen3"))
  Runs(tmp_path).run(
      "s1",
      "r1", [_cc_claude_result("oc-sess-1")],
      backend="oc-qwen",
      model="qwen/qwen3",
      native_session_id="oc-sess-1",
      started_at="2026-09-27T17:50:39.124791Z")
  oc = Opencode(tmp_path)
  with UsageLedger(tmp_path / "ledger.sqlite3") as ledger:
    assert _capture(ledger)["charlie-bot"] == 1
    row = _ledger_rows(ledger)["charlie-bot", "qwen3"]
    assert (row.calls, row.fallback_calls, row.in_fresh, row.cache_write, row.cache_read,
            row.output) == (1, 1, 4, 10, 100, 7)

    oc.write("m1", 100, _usage(4, 7), session="oc-sess-1")
    assert _capture(ledger)["opencode"] == 1
    assert ("charlie-bot", "qwen3") not in _ledger_rows(ledger)  # the opencode session is captured: retired


def test_a_capture_commits_once_and_a_failed_one_keeps_its_finished_files(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """No file of a capture is visible to another connection until the capture ends, the stamp
  lands in the same commit, and a capture that raises still commits the files it finished
  (without the stamp)."""
  claude, _codex = _claude_codex_corpus(tmp_path)
  path = tmp_path / "ledger.sqlite3"
  seen: list[int] = []
  real_read = claude_logs.read

  def spying_read(file: Path, account: str, previous: str | None) -> tuple:
    seen.append(len(reader.captured_sigs("host-a")))
    return real_read(file, account, previous)

  monkeypatch.setattr(claude_logs, "read", spying_read)
  with UsageLedger(path) as ledger, UsageLedger(path) as reader:
    _capture(ledger)
    assert len(seen) >= 2 and set(seen) == {0}  # nothing was committed while files were still being read
    assert len(reader.captured_sigs("host-a")) == 5  # 4 Claude files + 1 rollout
    assert reader.last_capture_at("host-a") is not None

    monkeypatch.setattr(claude_logs, "read", real_read)
    with (claude.work / "projects" / "rel" / "sess1" / "sess1.jsonl").open("a") as fh:
      fh.write(json.dumps(_claude_record("m3", NAME, "2024-01-05T00:00:00Z", _usage(1, 1))) + "\n")
    stamped = reader.last_capture_at("host-a")
    _write_rollout(Codex(tmp_path), "norootthread1", [_codex_meta(), _codex_turn("codex-m1"), _codex_count_solo("ts")])
    with pytest.raises(ValueError, match="norootthread1"):
      _capture(ledger)
    assert reader.last_capture_at("host-a") == stamped  # the failed capture left the stamp alone
    assert _ledger_row(reader, "Claude Code", NAME).calls == 3  # ... but the Claude file it finished is durable
