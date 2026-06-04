import json
import os
from pathlib import Path

import pytest

from src.agents.backends import tui


def test_ensure_claude_project_trusted_marks_session_dir(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  config_dir = tmp_path / "claude-config"
  working_dir = tmp_path / "session"
  working_dir.mkdir()
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))

  tui._ensure_claude_project_trusted(working_dir)

  data = json.loads((config_dir / ".claude.json").read_text(encoding="utf-8"))
  project = data["projects"][str(working_dir.resolve())]
  assert project["hasTrustDialogAccepted"] is True
  assert project["projectOnboardingSeenCount"] == 1


@pytest.mark.asyncio
async def test_ensure_tmux_session_uses_claude_tui_startup_args(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  config_dir = tmp_path / "claude-config"
  home_dir = tmp_path / "home"
  working_dir = tmp_path / "session"
  calls = []
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))
  monkeypatch.setattr(tui.Path, "home", staticmethod(lambda: home_dir))

  async def fake_run_tmux(*args: str, check: bool = False) -> tuple[int, str]:
    calls.append(args)
    if args[0] == "has-session":
      return 1, ""
    return 0, ""

  monkeypatch.setattr(tui, "_run_tmux", fake_run_tmux)

  await tui.ensure_tmux_session("session-id", working_dir)

  new_session_call = next(args for args in calls if args[0] == "new-session")
  assert new_session_call[-6:] == (
      "claude",
      "--settings",
      '{"skipDangerousModePermissionPrompt":true}',
      "--dangerously-skip-permissions",
      "--session-id",
      "session-id",
  )
  data = json.loads((config_dir / ".claude.json").read_text(encoding="utf-8"))
  assert data["projects"][str(working_dir.resolve())]["hasTrustDialogAccepted"] is True


@pytest.mark.asyncio
async def test_ensure_tmux_session_resumes_when_claude_jsonl_exists(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  config_dir = tmp_path / "claude-config"
  home_dir = tmp_path / "home"
  jsonl_path = home_dir / ".claude" / "projects" / "project-a" / "session-id.jsonl"
  jsonl_path.parent.mkdir(parents=True)
  jsonl_path.write_text("", encoding="utf-8")
  working_dir = tmp_path / "session"
  calls = []
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))
  monkeypatch.setattr(tui.Path, "home", staticmethod(lambda: home_dir))

  async def fake_run_tmux(*args: str, check: bool = False) -> tuple[int, str]:
    calls.append(args)
    if args[0] == "has-session":
      return 1, ""
    return 0, ""

  monkeypatch.setattr(tui, "_run_tmux", fake_run_tmux)

  await tui.ensure_tmux_session("session-id", working_dir)

  new_session_call = next(args for args in calls if args[0] == "new-session")
  assert "--resume" in new_session_call
  assert "--session-id" not in new_session_call
  assert new_session_call[-2:] == ("--resume", "session-id")


@pytest.mark.asyncio
async def test_ensure_tmux_session_passes_optional_claude_args(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  config_dir = tmp_path / "claude-config"
  home_dir = tmp_path / "home"
  working_dir = tmp_path / "session"
  calls = []
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))
  monkeypatch.setattr(tui.Path, "home", staticmethod(lambda: home_dir))

  async def fake_run_tmux(*args: str, check: bool = False) -> tuple[int, str]:
    calls.append(args)
    if args[0] == "has-session":
      return 1, ""
    return 0, ""

  monkeypatch.setattr(tui, "_run_tmux", fake_run_tmux)

  await tui.ensure_tmux_session(
      "session-id",
      working_dir,
      model="claude-opus-4-8",
      effort="max",
      disallowed_tools=["Monitor,CronCreate"],
  )

  new_session_call = next(args for args in calls if args[0] == "new-session")
  assert new_session_call[-6:] == (
      "--model",
      "claude-opus-4-8",
      "--effort",
      "max",
      "--disallowed-tools",
      "Monitor,CronCreate",
  )


def test_find_existing_claude_jsonl_returns_first_match(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  home_dir = tmp_path / "home"
  first = home_dir / ".claude" / "projects" / "a" / "session-id.jsonl"
  first.parent.mkdir(parents=True)
  first.write_text("", encoding="utf-8")
  monkeypatch.setattr(tui.Path, "home", staticmethod(lambda: home_dir))

  assert tui._find_existing_claude_jsonl("session-id") == first


def test_claude_jsonl_busy_uses_recent_mtime(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  home_dir = tmp_path / "home"
  jsonl_path = home_dir / ".claude" / "projects" / "a" / "session-id.jsonl"
  jsonl_path.parent.mkdir(parents=True)
  jsonl_path.write_text("", encoding="utf-8")
  os.utime(jsonl_path, (98.0, 98.0))
  monkeypatch.setattr(tui.Path, "home", staticmethod(lambda: home_dir))
  monkeypatch.setattr(tui.time, "time", lambda: 100.0)

  assert tui._claude_jsonl_busy("session-id") is True
  assert tui._claude_jsonl_busy("session-id", threshold_seconds=1.0) is False


def test_claude_jsonl_busy_returns_false_when_jsonl_missing(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
  home_dir = tmp_path / "home"
  monkeypatch.setattr(tui.Path, "home", staticmethod(lambda: home_dir))

  assert tui._claude_jsonl_busy("missing-session") is False


def test_claude_jsonl_busy_surfaces_stat_errors(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  missing_jsonl = tmp_path / "missing.jsonl"
  monkeypatch.setattr(tui, "_find_existing_claude_jsonl", lambda _: missing_jsonl)

  with pytest.raises(FileNotFoundError):
    tui._claude_jsonl_busy("session-id")
