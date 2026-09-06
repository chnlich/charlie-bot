"""Per-project instruction loading in the shared master builder.

An enabled project is ``<home>/projects/<group>/project.yaml`` (see
``src/core/project_config.py``): every grouped session's turn injects the
common rule body in full; a role=project manager additionally gets the repo
contract (``prompts/project_manager.md``) in full — replacing the pointer
identity part unenabled managers keep — plus the optional local supplement.
A present-but-invalid config or an unreadable applicable body fails the turn
(:class:`ProjectInstructionError` riding ``project_error``); a missing
project.yaml keeps the pre-project behavior exactly. All fixtures are fake
trees under ``tmp_path``; no live ``~/.charliebot`` state is touched.
"""

import hashlib
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import (
  BUILD_BACKEND_PATCH_TARGET,
  FakeBackend,
  make_work_item,
  mock_session_callbacks,
)
from structlog.testing import capture_logs

from src.agents import master_cc
from src.core.models import PROJECT_ROLE, BackendOption, SessionMetadata
from src.core.project_config import ProjectConfig

CONTRACT_MARK = "MANAGER CONTRACT BODY"
COMMON_MARK = "COMMON RULES BODY"
SUPPLEMENT_MARK = "MANAGER SUPPLEMENT BODY"


def _make_cfg(tmp_path: Path) -> SimpleNamespace:
  """Fake instruction inputs: repo base prompt + repo manager contract, no host override."""
  home = tmp_path / "home"
  repo = tmp_path / "repo"
  (repo / "prompts").mkdir(parents=True)
  (repo / "prompts" / "master.md").write_text("BASE PROMPT", encoding="utf-8")
  (repo / "prompts" / "project_manager.md").write_text(CONTRACT_MARK, encoding="utf-8")
  return SimpleNamespace(
      charlie_bot_repo=repo,
      claude_md_file=home / "MASTER_AGENT_PROMPT.md",
      memory_dir=home / "memory",
      charliebot_home=home,
  )


def _make_project(
    tmp_path: Path,
    *,
    yaml_body: str | None = None,
    files: dict[str, str] | None = None,
    group: str = "proj",
    write_contract: bool = True,
) -> SimpleNamespace:
  """An enabled fake project: project.yaml plus the body files it names."""
  cfg = _make_cfg(tmp_path)
  if not write_contract:
    (cfg.charlie_bot_repo / "prompts" / "project_manager.md").unlink()
  project_dir = cfg.charliebot_home / "projects" / group
  project_dir.mkdir(parents=True)
  if yaml_body is None:
    yaml_body = "prompt_file: common.md\nmanager_prompt_file: manager.md\n"
  (project_dir / "project.yaml").write_text(yaml_body, encoding="utf-8")
  for name, text in (files or {"common.md": COMMON_MARK, "manager.md": SUPPLEMENT_MARK}).items():
    (project_dir / name).write_text(text, encoding="utf-8")
  return cfg


def _meta(role: str | None, group: str | None) -> SimpleNamespace:
  return SimpleNamespace(id="s1", role=role, group=group)


# ---------------------------------------------------------------------------
# ProjectConfig: field validation
# ---------------------------------------------------------------------------


def test_prompt_file_required_and_nonempty() -> None:
  with pytest.raises(Exception, match="prompt_file"):
    ProjectConfig()
  with pytest.raises(Exception, match="nonempty"):
    ProjectConfig(prompt_file="")
  with pytest.raises(Exception, match="nonempty"):
    ProjectConfig(prompt_file="   ")


def test_manager_prompt_file_optional_but_nonempty_when_present() -> None:
  assert ProjectConfig(prompt_file="project.md").manager_prompt_file is None
  with pytest.raises(Exception, match="nonempty"):
    ProjectConfig(prompt_file="project.md", manager_prompt_file="")
  with pytest.raises(Exception, match="nonempty"):
    ProjectConfig(prompt_file="project.md", manager_prompt_file=" \n")


def test_unknown_keys_and_wrong_types_are_invalid() -> None:
  with pytest.raises(Exception, match="worker_prompt_file"):
    ProjectConfig(prompt_file="project.md", worker_prompt_file="w.md")
  with pytest.raises(Exception):
    ProjectConfig(prompt_file=123)
  with pytest.raises(Exception):
    ProjectConfig(prompt_file="project.md", manager_prompt_file=["manager.md"])


# ---------------------------------------------------------------------------
# Discovery and injection
# ---------------------------------------------------------------------------


def test_missing_project_config_keeps_prior_behavior(tmp_path: Path) -> None:
  cfg = _make_cfg(tmp_path)

  ordinary = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert ordinary is not None
  assert COMMON_MARK not in ordinary
  assert "# Project Manager session" not in ordinary

  manager = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  assert manager is not None
  assert CONTRACT_MARK not in manager  # pointer, not the contract itself
  assert "This session is the Project Manager for group proj." in manager
  assert "prompts/project_manager.md in the charlie-bot repo" in manager


def test_no_group_gets_no_project_layer(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path)
  out = master_cc._build_instructions_content(_meta(None, None), cfg, None)
  assert out is not None
  assert COMMON_MARK not in out
  assert "# Project Manager session" not in out


def test_ordinary_session_gets_common_body_once(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path)
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert out is not None
  assert out.count(COMMON_MARK) == 1
  assert "# Project Manager session" not in out
  assert CONTRACT_MARK not in out
  assert SUPPLEMENT_MARK not in out
  assert out.index("BASE PROMPT") < out.index(COMMON_MARK)


def test_manager_gets_contract_common_supplement_exactly_once_in_order(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path)
  out = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  assert out is not None
  # Group identity retained; the read-the-repo-file pointer dropped.
  assert "This session is the Project Manager for group proj." in out
  assert "prompts/project_manager.md in the charlie-bot repo" not in out
  assert out.count(CONTRACT_MARK) == 1
  assert out.count(COMMON_MARK) == 1
  assert out.count(SUPPLEMENT_MARK) == 1
  assert out.index("BASE PROMPT") < out.index(CONTRACT_MARK) < out.index(COMMON_MARK) < out.index(SUPPLEMENT_MARK)


def test_manager_without_configured_supplement_builds(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="prompt_file: common.md\n", files={"common.md": COMMON_MARK})
  out = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  assert out is not None
  assert out.count(COMMON_MARK) == 1
  assert out.count(CONTRACT_MARK) == 1
  assert SUPPLEMENT_MARK not in out


def test_global_and_overlay_content_preserved_with_project_layer(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path)
  overlay = cfg.charlie_bot_repo / "prompts" / "model_overlays"
  overlay.mkdir(parents=True)
  (overlay / "fence.md").write_text("OVERLAY FENCE", encoding="utf-8")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, "fence")
  assert out is not None
  assert "BASE PROMPT" in out
  assert COMMON_MARK in out
  assert "OVERLAY FENCE" in out
  assert out.index(COMMON_MARK) < out.index("OVERLAY FENCE")
  assert getattr(out, "overlay_error") is None


def test_body_diagnostics_log_source_path_and_hash(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path)
  with capture_logs() as logs:
    master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  loaded = [e for e in logs if e["event"] == "master_project_body_loaded"]
  assert len(loaded) == 1
  entry = loaded[0]
  assert entry["kind"] == "project_common"
  assert entry["path"] == str(cfg.charliebot_home / "projects" / "proj" / "common.md")
  assert entry["sha256"] == hashlib.sha256(COMMON_MARK.encode("utf-8")).hexdigest()
  assert "COMMON RULES BODY" not in str(entry)  # hash and path only, never the text


def test_hot_change_read_on_next_build(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path)
  common = cfg.charliebot_home / "projects" / "proj" / "common.md"

  first = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert first is not None and COMMON_MARK in first

  common.write_text("REPLACED COMMON BODY", encoding="utf-8")
  second = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert second is not None
  assert "REPLACED COMMON BODY" in second
  assert COMMON_MARK not in second


# ---------------------------------------------------------------------------
# Invalid config and unreadable bodies
# ---------------------------------------------------------------------------


def _assert_project_error(out: object, *fragments: str) -> None:
  assert out is not None
  error = getattr(out, "project_error")
  assert error is not None
  for fragment in fragments:
    assert fragment in str(error)


def test_invalid_yaml_config_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="prompt_file: [unclosed\n")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "project.yaml")


def test_non_mapping_config_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="- a\n- b\n")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "mapping")


def test_unknown_key_in_config_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="prompt_file: common.md\nworker_prompt_file: w.md\n")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "worker_prompt_file")


def test_wrong_type_in_config_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="prompt_file: 123\n")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "123")


def test_missing_common_body_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, files={"manager.md": SUPPLEMENT_MARK})
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "common.md")


def test_duplicate_body_destinations_fail(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="prompt_file: common.md\nmanager_prompt_file: common.md\n")
  out = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  _assert_project_error(out, "same file")


def test_body_path_outside_project_dir_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="prompt_file: ../outside.md\n", files={"common.md": COMMON_MARK})
  outside = cfg.charliebot_home / "outside.md"
  outside.write_text("OUTSIDE", encoding="utf-8")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "outside the project directory")
  assert "OUTSIDE" not in str(out)


def test_absolute_body_path_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body=f"prompt_file: {tmp_path / 'abs.md'}\n", files={"common.md": COMMON_MARK})
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "outside the project directory")


def test_symlink_escaping_project_dir_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, files={"common.md": COMMON_MARK})
  outside = tmp_path / "outside-real.md"
  outside.write_text("OUTSIDE", encoding="utf-8")
  link = cfg.charliebot_home / "projects" / "proj" / "common.md"
  link.unlink()
  link.symlink_to(outside)
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "outside the project directory")
  assert "OUTSIDE" not in str(out)


def test_unreadable_body_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path)
  (cfg.charliebot_home / "projects" / "proj" / "common.md").chmod(0o000)
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "unreadable")


def test_group_traversal_fails(tmp_path: Path) -> None:
  cfg = _make_cfg(tmp_path)
  escaped = cfg.charliebot_home / "evil"
  escaped.mkdir(parents=True)
  (escaped / "project.yaml").write_text("prompt_file: common.md\n", encoding="utf-8")
  (escaped / "common.md").write_text("EVIL RULES", encoding="utf-8")
  out = master_cc._build_instructions_content(_meta(None, "../evil"), cfg, None)
  _assert_project_error(out, "not a safe project directory name")
  assert "EVIL RULES" not in str(out)


# ---------------------------------------------------------------------------
# Manager-specific failures vs ordinary sessions
# ---------------------------------------------------------------------------


def test_missing_repo_contract_fails_manager_only(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, write_contract=False)
  ordinary = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert ordinary is not None
  assert getattr(ordinary, "project_error") is None
  assert COMMON_MARK in ordinary

  manager = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  _assert_project_error(manager, "repo manager contract unreadable")


def test_configured_supplement_missing_fails_manager_only(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, files={"common.md": COMMON_MARK})
  ordinary = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert ordinary is not None
  assert getattr(ordinary, "project_error") is None

  manager = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  _assert_project_error(manager, "manager.md", "unreadable")


# ---------------------------------------------------------------------------
# The wake path fails the turn on a project error
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_run_cc_fails_turn_on_project_error(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """An enabled project with an unreadable body fails the turn before any backend spawn."""
  cfg = _make_project(tmp_path, files={"manager.md": SUPPLEMENT_MARK})  # common.md missing
  cfg = SimpleNamespace(**{**vars(cfg), "sessions_dir": tmp_path / "sessions", "subprocess_buffer_limit": 1024})
  cfg.sessions_dir.mkdir()
  session_meta = SessionMetadata(id="s1", name="Researcher", group="proj")
  option = BackendOption(id="agy", label="Antigravity", type="antigravity", prompt_overlay="none")

  built: dict[str, bool] = {"called": False}

  def fake_build_backend(*args: object, **kwargs: object):
    built["called"] = True
    return FakeBackend()

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, fake_build_backend)
  callbacks = mock_session_callbacks()
  item = make_work_item(cfg, session_meta, option, callbacks=callbacks)

  cc_session_id, exit_code, error_msg, _extras = await master_cc._run_cc(item)

  assert built["called"] is False
  assert cc_session_id is None
  assert exit_code == 1
  assert error_msg is not None and "project instruction loading failed" in error_msg
  error_events = [
      c.args[1] for c in callbacks.persist_and_broadcast.await_args_list  # type: ignore[attr-defined]
      if c.args and isinstance(c.args[1], dict) and c.args[1].get("type") == "assistant_error"
  ]
  assert len(error_events) == 1
  assert "project instruction loading failed" in error_events[0]["content"]
  callbacks.mark_unread.assert_awaited_once_with("s1")


@pytest.mark.asyncio
async def test_run_cc_success_with_enabled_project(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  """A valid enabled project does not block the turn; instructions carry the common body."""
  cfg = _make_project(tmp_path)
  cfg = SimpleNamespace(**{**vars(cfg), "sessions_dir": tmp_path / "sessions", "subprocess_buffer_limit": 1024})
  cfg.sessions_dir.mkdir()
  session_meta = SessionMetadata(id="s1", name="Researcher", group="proj")
  option = BackendOption(id="agy", label="Antigravity", type="antigravity", prompt_overlay="none")

  def fake_build_backend(*args: object, **kwargs: object):
    return FakeBackend()

  monkeypatch.setattr(BUILD_BACKEND_PATCH_TARGET, fake_build_backend)
  item = make_work_item(cfg, session_meta, option)
  cc_session_id, exit_code, error_msg, _extras = await master_cc._run_cc(item)

  assert error_msg is None
  assert exit_code == 0
  assert cc_session_id is None
