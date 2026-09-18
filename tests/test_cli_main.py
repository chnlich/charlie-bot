"""Tests for the unified charliebot CLI dispatcher."""

import re
import sys
from types import ModuleType

import pytest
from conftest import ROOT

from src.cli import delegate, improve, publish, remote_launch, schedule_trigger, slack
from src.cli import main as cli_main
from src.cli.main import _COMMANDS


@pytest.mark.parametrize(
    ("subcommand", "module"),
    [
        ("delegate", delegate),
        ("improve", improve),
        ("schedule-trigger", schedule_trigger),
        ("remote-launch", remote_launch),
        ("publish", publish),
        ("slack", slack),
    ],
)
def test_dispatcher_delegates_to_supported_subcommands(
    monkeypatch: pytest.MonkeyPatch,
    subcommand: str,
    module: ModuleType,
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


def test_readme_cli_glance_names_exactly_the_dispatcher_subcommands() -> None:
  """The README "CLI at a glance" bullets stay in lockstep with _COMMANDS, the
  dispatcher registry that owns the subcommand vocabulary."""
  readme = ROOT.joinpath("README.md").read_text(encoding="utf-8")
  section = readme.split("## CLI at a glance", 1)[1].split("\n## ", 1)[0]
  documented = re.findall(r"^- `charliebot ([a-z-]+)`", section, flags=re.MULTILINE)
  assert sorted(documented) == sorted(_COMMANDS)
