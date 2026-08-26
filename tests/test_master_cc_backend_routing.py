import os
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import FakeBackend, make_work_item, patch_instructions_content
from conftest import make_transcript as _make_transcript
from structlog.testing import capture_logs

from src.agents import master_cc, master_cc_run
from src.agents.backends import base as backend_base
from src.core import config as core_config
from src.core import models


def test_build_master_env_removes_session_env_and_prepends_repo_venv(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  repo = tmp_path / "repo"
  venv_bin = repo / ".venv" / "bin"
  venv_bin.mkdir(parents=True)
  cfg = SimpleNamespace(charliebot_home=tmp_path / "home", charlie_bot_repo=repo)

  monkeypatch.setenv("PATH", "/usr/bin")
  monkeypatch.setenv("CLAUDECODE", "1")
  monkeypatch.setenv("CHARLIEBOT_SESSION_ID", "stale-session")

  env = master_cc._build_master_env(cfg)

  assert "CHARLIEBOT_SESSION_ID" not in env
  assert env["GIT_CEILING_DIRECTORIES"] == str(tmp_path / "home")
  assert env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] == "1"
  assert env["PATH"].split(os.pathsep)[:2] == [str(venv_bin), "/usr/bin"]
  assert "CLAUDECODE" not in env


def test_route_resume_session_uses_native_resume_id_for_charlie_code() -> None:
  assert master_cc._route_resume_session("charlie-code", "existing-session-id") == (
      [],
      "existing-session-id",
  )


def test_route_resume_session_uses_native_resume_id_for_antigravity() -> None:
  assert master_cc._route_resume_session("antigravity", "existing-session-id") == (
      [],
      "existing-session-id",
  )


def test_antigravity_is_resume_capable() -> None:
  assert "antigravity" in master_cc_run._RESUME_CAPABLE_BACKEND_TYPES


@pytest.mark.asyncio
async def test_run_cc_routes_antigravity_native_resume_id(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
  )
  session_meta = models.SessionMetadata(
      id="session-id",
      name="Antigravity",
      cc_session_id="existing-session-id",
      backend="agy",
  )
  backend_option = cfg.backend_options[0]
  captures: dict[str, object] = {}

  def fake_build_backend(option: models.BackendOption, cfg: core_config.CharlieBotConfig, **kwargs):
    captures["option"] = option
    captures["kwargs"] = kwargs
    return FakeBackend()

  monkeypatch.setattr("src.agents.backends.registry.build_backend", fake_build_backend)
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, backend_option)

  cc_session_id, exit_code, error_msg, _finish_extras = await master_cc._run_cc(item)

  assert captures["option"] is backend_option
  assert backend_option.model is None
  backend_kwargs = captures["kwargs"]
  assert isinstance(backend_kwargs, dict)
  assert backend_kwargs["extra_flags"] is None
  assert backend_kwargs["resume_session_id"] == "existing-session-id"
  assert cc_session_id == "existing-session-id"
  assert exit_code == 0
  assert error_msg is None


class _SessionIdBackend(FakeBackend):

  def __init__(self, session_id: str):
    self._session_id = session_id

  async def run(self, prompt: str, cwd: str, env: dict):
    yield {"session_id": self._session_id}
    yield backend_base.make_result_event()


class _AnchorMismatchBackend(FakeBackend):
  async def run(self, prompt: str, cwd: str, env: dict):
    yield backend_base.make_error_event("agy resume envelope id fresh-id does not match anchor anchor-id")
    raise ValueError("antigravity envelope guard: resume envelope id fresh-id does not match anchor anchor-id")


@pytest.mark.asyncio
async def test_run_cc_chain_adopts_session_id_and_resumes_with_it(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
  )
  patch_instructions_content(monkeypatch)

  # Run 1: a fresh antigravity backend emits a bare session_id event, which the
  # master adopts as the anchor.
  captures: dict[str, object] = {}
  backend_instances: list[object] = []

  def fake_build_backend(option, cfg, **kwargs):
    captures["kwargs"] = kwargs
    instance = _SessionIdBackend("conv-abc")
    backend_instances.append(instance)
    return instance

  monkeypatch.setattr("src.agents.backends.registry.build_backend", fake_build_backend)

  fresh_meta = models.SessionMetadata(id="session-id", name="Antigravity", backend="agy")
  item1 = make_work_item(cfg, fresh_meta, cfg.backend_options[0])
  cc_session_id, exit_code, error_msg, _ = await master_cc._run_cc(item1)

  assert cc_session_id == "conv-abc"
  assert exit_code == 0
  assert error_msg is None

  # Run 2: the anchored session passes the anchor through as the resume id.
  anchored_meta = models.SessionMetadata(
      id="session-id", name="Antigravity", backend="agy", cc_session_id="conv-abc")
  item2 = make_work_item(cfg, anchored_meta, cfg.backend_options[0])
  await master_cc._run_cc(item2)

  assert captures["kwargs"]["resume_session_id"] == "conv-abc"


@pytest.mark.asyncio
async def test_run_cc_guard_round_fails_with_guard_reason(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="agy", label="Antigravity", type="antigravity"),
      ],
  )
  session_meta = models.SessionMetadata(
      id="session-id", name="Antigravity", backend="agy", cc_session_id="anchor-id")

  monkeypatch.setattr(
      "src.agents.backends.registry.build_backend",
      lambda option, cfg, **kw: _AnchorMismatchBackend())
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, cfg.backend_options[0])

  cc_session_id, exit_code, error_msg, _finish_extras = await master_cc._run_cc(item)

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
      backend_options=[
          models.BackendOption(id="cc", label="CC", type="cc-claude", model="claude-fable-5"),
      ],
  )
  session_meta = models.SessionMetadata(id="session-id", name="CC", backend="cc")
  backend_option = cfg.backend_options[0]
  captures: dict[str, object] = {}

  def fake_build_backend(option, cfg, **kwargs):
    captures["kwargs"] = kwargs
    return FakeBackend()

  monkeypatch.setattr("src.agents.backends.registry.build_backend", fake_build_backend)
  patch_instructions_content(monkeypatch)

  item = make_work_item(cfg, session_meta, backend_option)

  await master_cc._run_cc(item)

  backend_kwargs = captures["kwargs"]
  assert backend_kwargs["extra_flags"] == ["--exclude-dynamic-system-prompt-sections"]


# ---------------------------------------------------------------------------
# resume_session log field: derived from the resolved resume id, honest on both
# the Claude family (--resume flag route) and native-resume backends.
# ---------------------------------------------------------------------------


def _starting_entry(logs: list[dict]) -> dict:
  matches = [e for e in logs if e.get("event") == "master_cc_starting"]
  assert len(matches) == 1, f"expected one master_cc_starting event, got {matches}"
  return matches[0]


async def _run_cc_starting_entry(
    cfg: core_config.CharlieBotConfig,
    session_meta: models.SessionMetadata,
    backend_option: models.BackendOption,
    monkeypatch: pytest.MonkeyPatch,
) -> dict:
  monkeypatch.setattr(
      "src.agents.backends.registry.build_backend",
      lambda option, cfg, **kw: FakeBackend())
  patch_instructions_content(monkeypatch)
  item = make_work_item(cfg, session_meta, backend_option)
  with capture_logs() as logs:
    await master_cc._run_cc(item)
  return _starting_entry(logs)


@pytest.mark.asyncio
async def test_claude_family_with_reachable_anchor_logs_resume_session_true(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  config_dir = tmp_path / "claude-config"
  _make_transcript(config_dir, "existing-session-id")
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(
              id="cc", label="CC", type="cc-claude", model="claude-fable-5",
              claude_config_dir=str(config_dir)),
      ],
  )
  session_meta = models.SessionMetadata(
      id="session-id", name="CC", backend="cc", cc_session_id="existing-session-id")

  entry = await _run_cc_starting_entry(cfg, session_meta, cfg.backend_options[0], monkeypatch)

  assert entry["resume_session"] is True


@pytest.mark.asyncio
async def test_claude_family_with_no_anchor_logs_resume_session_false(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="cc", label="CC", type="cc-claude", model="claude-fable-5"),
      ],
  )
  session_meta = models.SessionMetadata(id="session-id", name="CC", backend="cc")

  entry = await _run_cc_starting_entry(cfg, session_meta, cfg.backend_options[0], monkeypatch)

  assert entry["resume_session"] is False


@pytest.mark.asyncio
async def test_native_resume_backend_with_reachable_anchor_logs_resume_session_true(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
  cfg = core_config.CharlieBotConfig(
      charliebot_home=tmp_path / ".charliebot",
      backend_options=[
          models.BackendOption(id="oc", label="OpenCode", type="opencode", model="glm-5.2"),
      ],
  )
  session_meta = models.SessionMetadata(
      id="session-id", name="OpenCode", backend="oc", cc_session_id="existing-session-id")

  entry = await _run_cc_starting_entry(cfg, session_meta, cfg.backend_options[0], monkeypatch)

  assert entry["resume_session"] is True
