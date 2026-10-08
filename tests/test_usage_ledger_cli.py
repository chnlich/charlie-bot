"""Tests for the usage-ledger CLI (src/features/usage/cli.py).

Every ledger directory lives under tmp_path and --ledger names it, so no test reads the real
home directory or the real ledger; capture_local is monkeypatched because the real one would
read this host's own CLI logs. Each assertion checks a named mechanism (which ledger the
capture opens, the per-source output) rather than parser wording.
"""

from __future__ import annotations

import pathlib

import pytest

from src.app import main as cli_main
from src.features.usage import token_tally, usage_ledger


def _run(*argv: str) -> None:
  cli_main.main(["usage-ledger", *argv])


def test_capture_prints_one_line_per_source(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]) -> None:
  ledger_path = tmp_path / "ledger.sqlite3"
  opened: list[pathlib.Path] = []

  def fake_capture_local(ledger: usage_ledger.UsageLedger) -> dict[str, int]:
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
