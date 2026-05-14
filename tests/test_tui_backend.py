import json
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
  working_dir = tmp_path / "session"
  calls = []
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))

  async def fake_run_tmux(*args: str, check: bool = False) -> tuple[int, str]:
    calls.append(args)
    if args[0] == "has-session":
      return 1, ""
    return 0, ""

  monkeypatch.setattr(tui, "_run_tmux", fake_run_tmux)

  await tui.ensure_tmux_session("session-id", working_dir)

  new_session_call = next(args for args in calls if args[0] == "new-session")
  assert new_session_call[-4:] == (
      "claude",
      "--settings",
      '{"skipDangerousModePermissionPrompt":true}',
      "--dangerously-skip-permissions",
  )
  data = json.loads((config_dir / ".claude.json").read_text(encoding="utf-8"))
  assert data["projects"][str(working_dir.resolve())]["hasTrustDialogAccepted"] is True
