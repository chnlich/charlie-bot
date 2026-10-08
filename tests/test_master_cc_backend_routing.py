import os
from collections.abc import AsyncIterator
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import (
    AGY_BACKEND_OPTION,
    BUILD_BACKEND_PATCH_TARGET,
    CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET,
    FakeBackend,
    backend_option,
    make_work_item,
)

from src.features.chat_threads.thread_sessions import THREAD_CONTEXT_WINDOW
from src.features.slack.metadata import SlackOrigin
from src.infra import config as core_config
from src.infra import models
from src.infra.constants import SESSION_ID_ENV_VAR
from src.runtime import master_cc
from src.runtime.agent_process import base as backend_base
from src.runtime.hooks.backend_types import build_backend as real_build_backend


def build_antigravity_cfg(tmp_path: Path) -> core_config.CharlieBotConfig:
  """CharlieBotConfig for antigravity-routing tests: the .charliebot home lives under tmp_path so each
  test owns its own tree, and the backend list registers the model-less antigravity option the
  resume-id routing resolves against."""
  return core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [AGY_BACKEND_OPTION]},
  )


def test_build_master_env_writes_own_session_and_keeps_inherited_path(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  """The master's own session id lands in the child environment, over any inherited value, and the
  inherited PATH passes through without the repo venv's bin directory."""
  repo = tmp_path / "repo"
  venv_bin = repo / ".venv" / "bin"
  venv_bin.mkdir(parents=True)
  cfg = SimpleNamespace(charliebot_home=tmp_path / "home", charlie_bot_repo=repo)

  monkeypatch.setenv("PATH", "/usr/bin")
  monkeypatch.setenv("CLAUDECODE", "1")
  monkeypatch.setenv(SESSION_ID_ENV_VAR, "stale-session")

  env = master_cc.master_cc_run._build_master_env(cfg, "own-session")

  assert env[SESSION_ID_ENV_VAR] == "own-session"
  assert env["GIT_CEILING_DIRECTORIES"] == str(tmp_path / "home")
  assert env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] == "1"
  assert env["PATH"] == "/usr/bin"
  assert str(venv_bin) not in env["PATH"].split(os.pathsep)
  assert "CLAUDECODE" not in env


def build_charlie_code_cfg(tmp_path: Path) -> core_config.CharlieBotConfig:
  """CharlieBotConfig whose first entry is the shared charlie-code option: the entry thread
  sessions and main sessions resolve to, carrying its own (large) context window."""
  return core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={
          "options":
              [
                  backend_option(
                      id="charlie-code-kimi-k3",
                      label="Kimi-K3",
                      type="charlie-code",
                      model="moonshotai/Kimi-K3",
                      api_base="http://test.invalid/v1",
                      context_window=409_600,
                  )
              ]
      },
  )


def capturing_clc_build_backend(monkeypatch: pytest.MonkeyPatch, tmp_path: Path, captured: dict[str, object]):
  """A build_backend double that records the resolved option AND the real CLC command line
  built from it, while the run itself drives a FakeBackend (no subprocess)."""

  def fake_build_backend(
      option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs: object) -> FakeBackend:
    captured["option"] = option
    monkeypatch.setattr(CHARLIE_CODE_RESOLVE_BINARY_PATCH_TARGET, lambda name, fallback: "/usr/bin/charlie-code")
    real = real_build_backend(option, cfg)
    transport = tmp_path / "transport"
    transport.mkdir()
    real._prepare_transport(transport)
    captured["cmd"] = real._build_command("hello")
    return FakeBackend()

  return fake_build_backend


def command_window(cmd: list[str]) -> str:
  """The value following --context-window in a CLC command line."""
  return cmd[cmd.index("--context-window") + 1]


@pytest.mark.asyncio
async def test_run_cc_thread_session_pins_clc_context_window(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A thread session on a charlie-code option runs the fixed thread window: the resolved
  option carries THREAD_CONTEXT_WINDOW and the CLC command line passes it as
  --context-window, in place of the option's own value."""
  cfg = build_charlie_code_cfg(tmp_path)
  captured: dict[str, object] = {}
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, capturing_clc_build_backend(monkeypatch, tmp_path, captured))

  session_meta = models.SessionMetadata(profile="manager",
      id="session-id",
      name="Thread",
      backend="charlie-code-kimi-k3",
      slack_origin=SlackOrigin(team_id="T", channel_id="C", thread_ts="1700000000.000100"))
  item = make_work_item(cfg, session_meta, cfg.backends.options[0])
  await master_cc.master_cc_run._run_cc(item)

  assert captured["option"].context_window == THREAD_CONTEXT_WINDOW  # type: ignore[attr-defined]
  assert command_window(captured["cmd"]) == "96000"  # type: ignore[index]


@pytest.mark.asyncio
async def test_run_cc_main_session_keeps_option_context_window(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A main session on the same option keeps the option's own context window, on the resolved
  option and on the CLC command line alike."""
  cfg = build_charlie_code_cfg(tmp_path)
  captured: dict[str, object] = {}
  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, capturing_clc_build_backend(monkeypatch, tmp_path, captured))

  session_meta = models.SessionMetadata(profile="manager", id="session-id", name="Main", backend="charlie-code-kimi-k3")
  item = make_work_item(cfg, session_meta, cfg.backends.options[0])
  await master_cc.master_cc_run._run_cc(item)

  assert captured["option"].context_window == 409_600  # type: ignore[attr-defined]
  assert command_window(captured["cmd"]) == "409600"  # type: ignore[index]


def test_route_resume_session_uses_native_resume_id_for_charlie_code() -> None:
  assert master_cc.master_cc_run._route_resume_session("charlie-code", "existing-session-id") == (
      [],
      "existing-session-id",
  )


def test_route_resume_session_uses_native_resume_id_for_antigravity() -> None:
  assert master_cc.master_cc_run._route_resume_session("antigravity", "existing-session-id") == (
      [],
      "existing-session-id",
  )


class _SessionIdBackend(FakeBackend):

  def __init__(self, session_id: str) -> None:
    self._session_id = session_id

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    yield {"session_id": self._session_id}
    yield backend_base.make_result_event()


class _AnchorMismatchBackend(FakeBackend):

  async def run(self,
                prompt: str,
                cwd: str,
                env: dict,
                uploaded_files: list[dict] | None = None) -> AsyncIterator[dict]:
    yield backend_base.make_error_event("agy resume envelope id fresh-id does not match anchor anchor-id")
    raise ValueError("antigravity envelope guard: resume envelope id fresh-id does not match anchor anchor-id")


@pytest.mark.asyncio
async def test_run_cc_chain_adopts_session_id_and_resumes_with_it(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_antigravity_cfg(tmp_path)

  # Run 1: a fresh antigravity backend emits a bare session_id event, which the
  # master adopts as the anchor.
  captures: dict[str, object] = {}
  backend_instances: list[object] = []

  def fake_build_backend(
      option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs: object) -> _SessionIdBackend:
    captures["kwargs"] = kwargs
    instance = _SessionIdBackend("conv-abc")
    backend_instances.append(instance)
    return instance

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, fake_build_backend)

  fresh_meta = models.SessionMetadata(profile="manager", id="session-id", name="Antigravity", backend="agy")
  item1 = make_work_item(cfg, fresh_meta, cfg.backends.options[0])
  cc_session_id, exit_code, error_msg, _ = await master_cc.master_cc_run._run_cc(item1)

  assert cc_session_id == "conv-abc"
  assert exit_code == 0
  assert error_msg is None

  # Run 2: the anchored session passes the anchor through as the resume id.
  anchored_meta = models.SessionMetadata(profile="manager", id="session-id", name="Antigravity", backend="agy", cc_session_id="conv-abc")
  item2 = make_work_item(cfg, anchored_meta, cfg.backends.options[0])
  await master_cc.master_cc_run._run_cc(item2)

  assert captures["kwargs"]["resume_session_id"] == "conv-abc"


@pytest.mark.asyncio
async def test_run_cc_guard_round_fails_with_guard_reason(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = build_antigravity_cfg(tmp_path)
  session_meta = models.SessionMetadata(profile="manager", id="session-id", name="Antigravity", backend="agy", cc_session_id="anchor-id")

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, lambda option, cfg, **kw: _AnchorMismatchBackend())

  item = make_work_item(cfg, session_meta, cfg.backends.options[0])

  _cc_session_id, exit_code, error_msg, _finish_extras = await master_cc.master_cc_run._run_cc(item)

  assert exit_code != 0
  assert error_msg is not None
  assert "does not match anchor anchor-id" in error_msg


@pytest.mark.asyncio
async def test_run_cc_adds_exclude_dynamic_flag_for_cc_claude(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backends={"options": [backend_option(id="cc", label="CC", type="cc-claude", model="claude-fable-5"),]},
  )
  session_meta = models.SessionMetadata(profile="manager", id="session-id", name="CC", backend="cc")
  option = cfg.backends.options[0]
  captures: dict[str, object] = {}

  def fake_build_backend(
      option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs: object) -> FakeBackend:
    captures["kwargs"] = kwargs
    return FakeBackend()

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, fake_build_backend)

  item = make_work_item(cfg, session_meta, option)

  await master_cc.master_cc_run._run_cc(item)

  backend_kwargs = captures["kwargs"]
  assert backend_kwargs["extra_flags"] == ["--exclude-dynamic-system-prompt-sections"]
