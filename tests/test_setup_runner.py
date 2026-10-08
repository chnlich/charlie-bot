"""``python -m src.app.setup`` runs the home layout, then each registered setup step in registration order.

The stand-in steps below record their calls. ``--step`` calls one function and runs nothing else, which is how
a step re-enters in a fresh interpreter after ``uv sync``.
"""

import subprocess
from pathlib import Path

import pytest

from src.app import registrations
from src.app import setup as setup_runner
from src.backends.claude_code import setup_check
from src.features.cron import seed
from src.features.voice import voice_setup
from src.infra.config import CharlieBotConfig
from src.runtime.hooks import wiring

CALLS: list[tuple[str, bool, CharlieBotConfig]] = []


def first_step(cfg: CharlieBotConfig, *, dry_run: bool) -> None:
  CALLS.append(("first", dry_run, cfg))


def second_step(cfg: CharlieBotConfig, *, dry_run: bool) -> None:
  CALLS.append(("second", dry_run, cfg))


def lone_step(cfg: CharlieBotConfig, *, dry_run: bool) -> None:
  CALLS.append(("lone", dry_run, cfg))


@pytest.fixture
def fresh_registry(profile_home: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
  """A scratch CHARLIEBOT_HOME and a registry that holds only the two stand-in steps."""
  CALLS.clear()
  monkeypatch.setattr(registrations, "register_all", lambda: None)
  monkeypatch.setattr(wiring, "_SETUP_STEPS", [])
  wiring.register_setup_step(__name__, attr="first_step")
  wiring.register_setup_step(__name__, attr="second_step")
  return profile_home


@pytest.mark.parametrize(("argv", "dry_run"), [(["--dry-run"], True), ([], False)])
def test_the_runner_calls_each_step_in_registration_order_with_the_dry_run_flag(
    fresh_registry: Path, argv: list[str], dry_run: bool) -> None:
  setup_runner.main(argv)

  assert [(name, flag) for name, flag, _ in CALLS] == [("first", dry_run), ("second", dry_run)]
  assert all(cfg.charliebot_home == fresh_registry for _, _, cfg in CALLS)
  # A dry run writes nothing: the home layout step has not created config.yaml.
  assert (fresh_registry / "config.yaml").exists() is (not dry_run)


def test_step_calls_an_unregistered_function_once_and_runs_nothing_else(fresh_registry: Path) -> None:
  setup_runner.main(["--step", f"{__name__}:lone_step"])

  assert [(name, flag) for name, flag, _ in CALLS] == [("lone", False)]
  assert not (fresh_registry / "config.yaml").exists()


def test_the_packages_register_the_cron_voice_and_claude_code_steps_in_that_order() -> None:
  registrations.register_all()

  assert wiring.setup_steps() == [seed.setup_step, voice_setup.setup_step, setup_check.setup_step]


@pytest.fixture
def recorded_commands(monkeypatch: pytest.MonkeyPatch) -> list[list[str]]:
  commands: list[list[str]] = []
  monkeypatch.setattr(subprocess, "run", lambda command, **kwargs: commands.append(command))
  return commands


def test_the_voice_step_syncs_then_re_enters_only_on_enable_step_when_a_gpu_is_present(
    monkeypatch: pytest.MonkeyPatch, recorded_commands: list[list[str]]) -> None:
  monkeypatch.setattr(voice_setup.shutil, "which", lambda name: "/usr/bin/nvidia-smi")

  voice_setup.setup_step(CharlieBotConfig(), dry_run=False)

  assert recorded_commands[0] == ["uv", "sync", "--group", "gpu-voice"]
  assert recorded_commands[1][:4] == ["uv", "run", "--no-sync", "python"]
  assert recorded_commands[1][-2:] == ["--step", "src.features.voice.voice_setup:enable_step"]
  assert len(recorded_commands) == 2


@pytest.mark.parametrize(("gpu", "dry_run"), [(False, False), (False, True), (True, True)])
def test_the_voice_step_runs_no_command_without_a_gpu_or_in_a_dry_run(
    monkeypatch: pytest.MonkeyPatch, recorded_commands: list[list[str]], gpu: bool, dry_run: bool) -> None:
  monkeypatch.setattr(voice_setup.shutil, "which", lambda name: "/usr/bin/nvidia-smi" if gpu else None)

  voice_setup.setup_step(CharlieBotConfig(), dry_run=dry_run)

  assert recorded_commands == []
