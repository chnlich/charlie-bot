"""Acceptance for the session-identity variable at the spawn boundary.

The identity a master's CLIs resolve is only as good as what reaches the agent
child's environment, so both legs read the variable back out of a real spawned
process: a fake `claude` shim dumps its own environment and the assertions run
against that dump, never against the constructor's return value.

  - master leg: a full ``_run_cc`` round through the real backend spawn path,
    with a stale value in the server's own environment, so the dump proves both
    the wiring (this session's id, not the inherited one) and the boundary.
  - worker leg: the environment ``worker.run`` builds (``claude_supervisor_env``
    over the process environment plus the worker's extras), spawned through the
    same backend, so the dump proves the strip travels to the child.
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest
from conftest import make_work_item, patch_instructions_content

from src.agents import master_cc
from src.agents.backends.claude_code import ClaudeCodeBackend, claude_supervisor_env
from src.core import config as core_config
from src.core import models

_SHIM_TEMPLATE = """#!/bin/sh
env > '{dump}'
echo '{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"SHIM"}}]}}}}'
echo '{{"type":"result","subtype":"success","is_error":false,"result":"SHIM","usage":{{"input_tokens":1,"output_tokens":1}}}}'
exit 0
"""


def _install_env_dump_shim(tmp_path: Path) -> tuple[Path, Path]:
  """Write a claude-shaped shim that dumps its own environment; returns (shim, dump path)."""
  shim_dir = tmp_path / "shim"
  shim_dir.mkdir()
  shim = shim_dir / "claude"
  dump = tmp_path / "child_env"
  shim.write_text(_SHIM_TEMPLATE.format(dump=dump), encoding="utf-8")
  shim.chmod(0o755)
  return shim, dump


def _read_env_dump(dump: Path) -> dict[str, str]:
  env: dict[str, str] = {}
  for line in dump.read_text(encoding="utf-8", errors="replace").splitlines():
    name, sep, value = line.partition("=")
    if sep:
      env[name] = value
  return env


@pytest.mark.asyncio
async def test_master_child_environment_carries_its_own_session_id(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  shim, dump = _install_env_dump_shim(tmp_path)
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(
              id="fake",
              label="Fake",
              type="cc-claude",
              model="fake-model",
              cli_binary=str(shim),
              prompt_overlay="none",
          )
      ],
  )
  (cfg.sessions_dir / "live-session").mkdir(parents=True)
  # A server started from inside another session's shell hands down a stale id.
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "stale-session")
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, models.SessionMetadata(id="live-session", name="Live"), cfg.backend_options[0])
  await master_cc._run_cc(item)

  assert _read_env_dump(dump)["CHARLIEBOT_SESSION_ID"] == "live-session"


@pytest.mark.asyncio
async def test_worker_child_environment_carries_no_session_id(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  shim, dump = _install_env_dump_shim(tmp_path)
  cwd = tmp_path / "worktree"
  cwd.mkdir()
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "stale-session")

  # The mapping src/agents/worker.py hands to the shared constructor.
  env = claude_supervisor_env({**os.environ, "CHARLIEBOT_TEST_EXTRA": "1"})
  backend = ClaudeCodeBackend(model="fake-model", cli_binary=str(shim), instructions_content="instructions")
  async for _event in backend.run("prompt", str(cwd), env):
    pass

  child_env = _read_env_dump(dump)
  assert "CHARLIEBOT_SESSION_ID" not in child_env
  assert child_env["CHARLIEBOT_TEST_EXTRA"] == "1"
