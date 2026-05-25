"""Tests for the unified charliebot CLI dispatcher."""

import sys

import pytest

from src.cli import delegate
from src.cli import improve
from src.cli import main as cli_main
from src.cli import remote_launch
from src.cli import schedule_trigger


@pytest.mark.parametrize(
    ("subcommand", "module"),
    [
        ("delegate", delegate),
        ("improve", improve),
        ("schedule-trigger", schedule_trigger),
        ("remote-launch", remote_launch),
    ],
)
def test_dispatcher_delegates_to_supported_subcommands(
    monkeypatch: pytest.MonkeyPatch,
    subcommand: str,
    module,
) -> None:
  calls: list[list[str]] = []

  def fake_main() -> None:
    calls.append(sys.argv.copy())

  monkeypatch.setattr(module, "main", fake_main)
  monkeypatch.setattr(sys, "argv", ["charliebot"])

  cli_main.main([subcommand, "--flag", "value"])

  assert calls == [[f"charliebot {subcommand}", "--flag", "value"]]
  assert sys.argv == ["charliebot"]


def test_dispatcher_prints_help_for_root_command(capsys: pytest.CaptureFixture[str]) -> None:
  cli_main.main(["--help"])

  out = capsys.readouterr().out
  assert "usage: charliebot <subcommand>" in out
  assert "delegate" in out
  assert "remote-launch" in out


def test_dispatcher_rejects_unknown_subcommand(capsys: pytest.CaptureFixture[str]) -> None:
  with pytest.raises(SystemExit) as exc_info:
    cli_main.main(["missing"])

  assert exc_info.value.code == 2
  err = capsys.readouterr().err
  assert "unknown subcommand" in err
  assert "schedule-trigger" in err
