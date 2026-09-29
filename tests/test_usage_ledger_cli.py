"""Tests for the usage-ledger CLI (src/cli/usage_ledger.py).

Every ledger and fixture directory lives under tmp_path and --ledger names it, so no test
reads the real home directory or the real ledger; capture_local is monkeypatched because
the real one would read this host's own CLI logs. Each assertion checks a named mechanism
(what lands in the ledger, the exit-2 contracts, the per-source output) rather than
parser wording.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.cli import main as cli_main
from src.core import token_tally
from src.core.usage_ledger import UsageLedger


def _write_claude_fixture(root: Path) -> Path:
  """A minimal copied Claude config dir: one project jsonl carrying one usage record."""
  home = root / "copied-claude"
  sess_dir = home / "projects" / "rel" / "sess1"
  sess_dir.mkdir(parents=True)
  record = {
      "message":
          {
              "id": "msg-1",
              "model": "claude-model",
              "usage":
                  {
                      "input_tokens": 10,
                      "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0,
                      "output_tokens": 5
                  },
          },
      "requestId": "req-msg-1",
      "uuid": "u-msg-1",
      "timestamp": "2026-01-01T00:00:00Z",
  }
  (sess_dir / "sess1.jsonl").write_text(json.dumps(record) + "\n")
  return home


def _run(*argv: str) -> None:
  cli_main.main(["usage-ledger", *argv])


def test_import_writes_the_fixture_then_zero(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  home = _write_claude_fixture(tmp_path)
  ledger_path = tmp_path / "ledger.sqlite3"
  argv = ["import", "--host", "old-host", "--claude-home", f"work={home}", "--ledger", str(ledger_path)]

  _run(*argv)
  assert "Claude Code: 1 records written" in capsys.readouterr().out
  with UsageLedger(ledger_path) as ledger:
    rows = ledger.model_rows()
  assert len(rows) == 1
  assert (rows[0].source, rows[0].calls, rows[0].output) == ("Claude Code", 1, 5)

  _run(*argv)
  assert "Claude Code: 0 records written" in capsys.readouterr().out


def test_malformed_pair_and_missing_source_flag_exit_2(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  ledger = str(tmp_path / "ledger.sqlite3")
  with pytest.raises(SystemExit) as malformed:
    _run("import", "--host", "old-host", "--claude-home", "no-separator", "--ledger", ledger)
  assert malformed.value.code == 2

  with pytest.raises(SystemExit) as missing_dir:
    _run("import", "--host", "old-host", "--claude-home", f"work={tmp_path / 'absent'}", "--ledger", ledger)
  assert missing_dir.value.code == 2

  with pytest.raises(SystemExit) as no_source:
    _run("import", "--host", "old-host", "--ledger", ledger)
  assert no_source.value.code == 2
  assert capsys.readouterr().err  # argparse's own message, not a silent exit


def test_capture_prints_one_line_per_source(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  ledger_path = tmp_path / "ledger.sqlite3"
  opened: list[Path] = []

  def fake_capture_local(ledger: UsageLedger) -> dict[str, int]:
    opened.append(ledger._path)
    return {"Claude Code": 2, "Codex": 1}

  monkeypatch.setattr(token_tally, "capture_local", fake_capture_local)
  _run("capture", "--ledger", str(ledger_path))
  assert opened == [ledger_path]  # the CLI's ledger is the one --ledger names
  assert capsys.readouterr().out.splitlines() == [
      "Claude Code: 2 records written",
      "Codex: 1 records written",
  ]


def test_top_level_help_lists_usage_ledger(capsys: pytest.CaptureFixture[str]) -> None:
  cli_main.main(["--help"])
  assert "usage-ledger" in capsys.readouterr().out
