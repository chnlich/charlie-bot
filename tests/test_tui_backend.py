from pathlib import Path

import pytest
from conftest import make_fake_run_tmux

from src.agents.backends import pty_common, tui


@pytest.fixture(autouse=True)
def _clear_jsonl_memo() -> None:
  # tmp-home monkeypatching differs per test; a stale memo entry would resolve
  # against another test's home.
  tui.reset_jsonl_memo_for_tests()


def test_build_claude_argv_joins_disallowed_tools_into_single_flag() -> None:
  argv = tui.build_claude_argv(
      "session-id",
      resume=False,
      settings=tui._CLAUDE_TUI_SETTINGS,
      disallowed_tools=["Monitor,CronCreate", "AskUserQuestion,ExitPlanMode"],
  )

  # The launched `claude` reliably honors one comma-joined flag, not repeated ones.
  assert argv.count("--disallowed-tools") == 1
  idx = argv.index("--disallowed-tools")
  assert argv[idx + 1] == "Monitor,CronCreate,AskUserQuestion,ExitPlanMode"


def _patch_tmux_env(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> tuple[Path, Path, list[tuple[str, ...]]]:
  """Redirect CLAUDE_CONFIG_DIR into tmp_path and tmux calls into fakes; return (config_dir, working_dir, tmux calls).

  ensure_tmux_session's tmux calls flow through pty_common globals (the has-session
  probe via tmux_session_exists, the spawn via _start_tmux_session); an unpatched
  pty_common global would reach the real tmux binary. Request the ``path_home``
  fixture alongside this rig when the code under test resolves ``Path.home()``.
  """
  config_dir = tmp_path / "claude-config"
  working_dir = tmp_path / "session"
  calls: list[tuple[str, ...]] = []
  monkeypatch.setenv("CLAUDE_CONFIG_DIR", str(config_dir))
  monkeypatch.setattr(pty_common, "_run_tmux", make_fake_run_tmux(calls))
  return config_dir, working_dir, calls


@pytest.mark.asyncio
async def test_ensure_tmux_session_injects_new_session_env(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    path_home: Path,
) -> None:
  _, working_dir, calls = _patch_tmux_env(monkeypatch, tmp_path)

  await tui.ensure_tmux_session(
      "session-id",
      working_dir,
      inject_env={
          "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS": "1",
          "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE": "forwarded-test-value",
      },
  )

  new_session_call = next(args for args in calls if args[0] == "new-session")
  command_index = new_session_call.index("claude")
  assert new_session_call[command_index - 4:command_index] == (
      "-e",
      "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1",
      "-e",
      "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE=forwarded-test-value",
  )
