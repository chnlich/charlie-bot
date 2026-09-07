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
import sys
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace

import pytest
from conftest import (
    BUILD_BACKEND_PATCH_TARGET,
    FakeBackend,
    make_instruction_cfg,
    make_work_item,
    mock_session_callbacks,
)
from structlog.testing import capture_logs

from src.agents import master_cc
from src.core.models import PROJECT_ROLE, BackendOption, SessionMetadata
from src.core.project_config import (
    ProjectConfig,
    ProjectInstructionError,
    load_project_bodies,
)

CONTRACT_MARK = "MANAGER CONTRACT BODY"
COMMON_MARK = "COMMON RULES BODY"
SUPPLEMENT_MARK = "MANAGER SUPPLEMENT BODY"


def _make_project(
    tmp_path: Path,
    *,
    yaml_body: str | None = None,
    files: dict[str, str] | None = None,
    group: str = "proj",
    write_contract: bool = True,
) -> SimpleNamespace:
  """An enabled fake project: project.yaml plus the body files it names."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)
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


def _seed_project_dir(tmp_path: Path) -> tuple[SimpleNamespace, Path]:
  """A fake home whose ``projects/proj`` directory exists and holds a valid common body; tests
  break one aspect of the config on top of it (a symlinked, malformed, or absent project.yaml)."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)
  project_dir = cfg.charliebot_home / "projects" / "proj"
  project_dir.mkdir(parents=True)
  (project_dir / "common.md").write_text(COMMON_MARK, encoding="utf-8")
  return cfg, project_dir


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


def test_manager_prompt_file_explicit_null_rejected() -> None:
  """An explicit null is invalid; only true omission of the key means "no supplement"."""
  with pytest.raises(Exception, match="not null"):
    ProjectConfig(prompt_file="project.md", manager_prompt_file=None)


def test_yaml_explicit_null_manager_prompt_file_fails_every_session(tmp_path: Path) -> None:
  """`manager_prompt_file: null` in the yaml is invalid for managers and ordinary sessions alike."""
  cfg = _make_project(tmp_path, yaml_body="prompt_file: common.md\nmanager_prompt_file: null\n")
  for meta in (_meta(None, "proj"), _meta(PROJECT_ROLE, "proj")):
    out = master_cc._build_instructions_content(meta, cfg, None)
    _assert_project_error(out, "not null")


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
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)

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


_BROKEN_YAML_CASES = [
    pytest.param("prompt_file: [unclosed\n", ("project.yaml",), id="invalid-yaml"),
    pytest.param("- a\n- b\n", ("mapping",), id="non-mapping-config"),
    pytest.param("prompt_file: common.md\nworker_prompt_file: w.md\n", ("worker_prompt_file",), id="unknown-key"),
    pytest.param("prompt_file: 123\n", ("123",), id="wrong-type"),
]


@pytest.mark.parametrize(("yaml_body", "error_fragments"), _BROKEN_YAML_CASES)
def test_broken_project_yaml_fails(tmp_path: Path, yaml_body: str, error_fragments: tuple[str, ...]) -> None:
  """A malformed project.yaml fails the turn with an error naming the cause."""
  cfg = _make_project(tmp_path, yaml_body=yaml_body)
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, *error_fragments)


def test_missing_common_body_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, files={"manager.md": SUPPLEMENT_MARK})
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "common.md")


def test_duplicate_body_destinations_fail(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="prompt_file: common.md\nmanager_prompt_file: common.md\n")
  out = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  _assert_project_error(out, "same file")


def test_duplicate_body_destinations_fail_ordinary_session(tmp_path: Path) -> None:
  """Destination validity applies to ordinary sessions, which never read the supplement."""
  cfg = _make_project(
      tmp_path, yaml_body="prompt_file: common.md\nmanager_prompt_file: common.md\n",
      files={"common.md": COMMON_MARK})  # manager.md absent: must fail without ever needing it
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "same file")


def test_duplicate_body_destinations_via_symlink_alias_fail(tmp_path: Path) -> None:
  """Two fields pointing at one file through a symlink alias are the same destination."""
  cfg, project_dir = _seed_project_dir(tmp_path)
  (project_dir / "alias.md").symlink_to(project_dir / "common.md")
  (project_dir / "project.yaml").write_text("prompt_file: common.md\nmanager_prompt_file: alias.md\n", encoding="utf-8")
  for meta in (_meta(None, "proj"), _meta(PROJECT_ROLE, "proj")):
    out = master_cc._build_instructions_content(meta, cfg, None)
    _assert_project_error(out, "same file")


def test_ordinary_session_does_not_read_supplement_content(tmp_path: Path) -> None:
  """An ordinary session neither reads nor requires the manager-only body."""
  cfg = _make_project(tmp_path)
  (cfg.charliebot_home / "projects" / "proj" / "manager.md").chmod(0o000)
  ordinary = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert ordinary is not None
  assert getattr(ordinary, "project_error") is None
  assert COMMON_MARK in ordinary
  assert SUPPLEMENT_MARK not in ordinary

  manager = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  _assert_project_error(manager, "manager.md", "unreadable")


def test_body_path_outside_project_dir_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body="prompt_file: ../outside.md\n", files={"common.md": COMMON_MARK})
  outside = cfg.charliebot_home / "outside.md"
  outside.write_text("OUTSIDE", encoding="utf-8")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "outside the project directory")
  assert "OUTSIDE" not in str(out)


def test_absolute_body_path_inside_project_dir_fails(tmp_path: Path) -> None:
  """An absolute path is invalid schema even when it points inside the project directory."""
  cfg, project_dir = _seed_project_dir(tmp_path)
  (project_dir / "project.yaml").write_text(f"prompt_file: {project_dir / 'common.md'}\n", encoding="utf-8")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "relative path")
  assert COMMON_MARK not in str(out)


def test_absolute_body_path_outside_project_dir_fails(tmp_path: Path) -> None:
  cfg = _make_project(tmp_path, yaml_body=f"prompt_file: {tmp_path / 'abs.md'}\n", files={"common.md": COMMON_MARK})
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "relative path")


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
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)
  escaped = cfg.charliebot_home / "evil"
  escaped.mkdir(parents=True)
  (escaped / "project.yaml").write_text("prompt_file: common.md\n", encoding="utf-8")
  (escaped / "common.md").write_text("EVIL RULES", encoding="utf-8")
  out = master_cc._build_instructions_content(_meta(None, "../evil"), cfg, None)
  _assert_project_error(out, "not a safe project directory name")
  assert "EVIL RULES" not in str(out)


# ---------------------------------------------------------------------------
# Config file present-but-broken states: never a silent disable
# ---------------------------------------------------------------------------


def test_dangling_config_symlink_fails_instead_of_disabling(tmp_path: Path) -> None:
  """A broken project.yaml symlink is a present-but-broken config, never "not enabled"."""
  cfg, project_dir = _seed_project_dir(tmp_path)
  (project_dir / "project.yaml").symlink_to(project_dir / "missing.yaml")

  with pytest.raises(ProjectInstructionError, match="broken symlink"):
    load_project_bodies(cfg.charliebot_home, "proj", manager=False)

  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "broken symlink")
  manager = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  _assert_project_error(manager, "broken symlink")
  # Not silently disabled: the unenabled-manager pointer identity stays absent.
  assert "This session is the Project Manager for group proj" not in str(manager)


def test_dangling_project_directory_symlink_fails_instead_of_disabling(tmp_path: Path) -> None:
  """A broken projects/<group> symlink is a present-but-broken directory, never "not enabled"."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)
  (cfg.charliebot_home / "projects").mkdir(parents=True)
  target = tmp_path / "gone-project"
  dangling = cfg.charliebot_home / "projects" / "proj"
  dangling.symlink_to(target)

  with pytest.raises(ProjectInstructionError, match="broken symlink"):
    load_project_bodies(cfg.charliebot_home, "proj", manager=False)

  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "broken symlink", str(dangling), str(target))
  manager = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  _assert_project_error(manager, "broken symlink", str(dangling), str(target))
  # Not silently disabled: the unenabled-manager pointer identity stays absent.
  assert "This session is the Project Manager for group proj" not in str(manager)


def test_project_directory_symlink_loop_fails_instead_of_disabling(tmp_path: Path) -> None:
  """An unresolvable project directory symlink is a config error, not a silent skip."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)
  (cfg.charliebot_home / "projects").mkdir(parents=True)
  loop = cfg.charliebot_home / "projects" / "proj"
  loop.symlink_to(loop)
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "broken symlink", str(loop))


def test_absent_project_directory_still_unconfigured(tmp_path: Path) -> None:
  """A truly absent project directory (projects/ present, group absent) keeps "not enabled"."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)
  (cfg.charliebot_home / "projects").mkdir(parents=True)
  assert load_project_bodies(cfg.charliebot_home, "proj", manager=False) is None
  assert load_project_bodies(cfg.charliebot_home, "proj", manager=True) is None

  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert out is not None
  assert getattr(out, "project_error") is None
  assert COMMON_MARK not in out
  manager = master_cc._build_instructions_content(_meta(PROJECT_ROLE, "proj"), cfg, None)
  assert manager is not None
  assert getattr(manager, "project_error") is None
  assert "This session is the Project Manager for group proj." in manager


def test_config_symlink_outside_project_dir_fails(tmp_path: Path) -> None:
  """A project.yaml symlink out of the project directory breaks the declared isolation."""
  cfg, project_dir = _seed_project_dir(tmp_path)
  outside = tmp_path / "elsewhere" / "project.yaml"
  outside.parent.mkdir()
  outside.write_text("prompt_file: common.md\n", encoding="utf-8")
  (project_dir / "project.yaml").symlink_to(outside)
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "outside the project directory")


def test_config_symlink_inside_project_dir_loads(tmp_path: Path) -> None:
  """An in-directory alias for project.yaml is a valid config location."""
  cfg, project_dir = _seed_project_dir(tmp_path)
  (project_dir / "real-config.yaml").write_text("prompt_file: common.md\n", encoding="utf-8")
  (project_dir / "project.yaml").symlink_to(project_dir / "real-config.yaml")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert out is not None
  assert getattr(out, "project_error") is None
  assert COMMON_MARK in out


def test_bad_utf8_config_fails(tmp_path: Path) -> None:
  """A non-UTF-8 config is a per-turn project error, not a raw UnicodeDecodeError escape."""
  cfg, project_dir = _seed_project_dir(tmp_path)
  (project_dir / "project.yaml").write_bytes(b"prompt_file: \xff\xfe.md\n")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "unreadable")


def test_config_directory_fails(tmp_path: Path) -> None:
  """project.yaml as a directory is an unreadable config, never a silent skip."""
  cfg, project_dir = _seed_project_dir(tmp_path)
  (project_dir / "project.yaml").mkdir()
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out, "unreadable")


def test_symlink_loop_body_resolution_fails(tmp_path: Path) -> None:
  """A symlink-loop body destination is a config error, not a raw resolve crash."""
  cfg, project_dir = _seed_project_dir(tmp_path)
  loop = project_dir / "common.md"
  loop.unlink()
  loop.symlink_to(loop)
  (project_dir / "project.yaml").write_text("prompt_file: common.md\n", encoding="utf-8")
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  # Where the loop fails splits on the interpreter: Python 3.12 resolve()
  # raises RuntimeError and the error reads "cannot be resolved"; 3.13
  # resolve() folds the loop into itself, so the loop surfaces as OSError at
  # the read and the error reads "body unreadable". Either way the turn
  # fails with a ProjectInstructionError naming the file.
  fragment = "body unreadable" if sys.version_info >= (3, 13) else "cannot be resolved"
  _assert_project_error(out, fragment)


def test_symlinked_project_directory_stays_confined(tmp_path: Path) -> None:
  """The project directory may itself be a symlink: bodies stay confined to its real target."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)
  real = tmp_path / "real-project"
  real.mkdir()
  (real / "common.md").write_text(COMMON_MARK, encoding="utf-8")
  (real / "manager.md").write_text(SUPPLEMENT_MARK, encoding="utf-8")
  (real / "project.yaml").write_text("prompt_file: common.md\nmanager_prompt_file: manager.md\n", encoding="utf-8")
  link = cfg.charliebot_home / "projects" / "proj"
  link.parent.mkdir(parents=True)
  link.symlink_to(real)
  out = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  assert out is not None
  assert getattr(out, "project_error") is None
  assert COMMON_MARK in out

  # A body symlink escaping the real directory still fails confinement.
  outside = tmp_path / "outside.md"
  outside.write_text("OUTSIDE", encoding="utf-8")
  (real / "common.md").unlink()
  (real / "common.md").symlink_to(outside)
  out2 = master_cc._build_instructions_content(_meta(None, "proj"), cfg, None)
  _assert_project_error(out2, "outside the project directory")
  assert "OUTSIDE" not in str(out2)


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


def _dangling_project_dir(tmp_path: Path) -> SimpleNamespace:
  """A projects/<group> directory that is a symlink to a missing target."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_MARK)
  (cfg.charliebot_home / "projects").mkdir(parents=True)
  (cfg.charliebot_home / "projects" / "proj").symlink_to(tmp_path / "gone-project")
  return cfg


_FAILING_PROJECT_CASES = [
    pytest.param(
        lambda tmp_path: _make_project(tmp_path, files={"manager.md": SUPPLEMENT_MARK}),
        ("project instruction loading failed",),
        id="unreadable-common-body"),
    pytest.param(
        lambda tmp_path: _make_project(
            tmp_path,
            yaml_body="prompt_file: common.md\nmanager_prompt_file: common.md\n",
            files={"common.md": COMMON_MARK}), ("same file",),
        id="duplicate-body-destinations"),
    pytest.param(
        _dangling_project_dir, ("project instruction loading failed", "broken symlink"),
        id="dangling-project-directory"),
]


@pytest.mark.asyncio
@pytest.mark.parametrize(("make_cfg", "error_fragments"), _FAILING_PROJECT_CASES)
async def test_run_cc_fails_turn_on_project_error(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    make_cfg: Callable[[Path], SimpleNamespace],
    error_fragments: tuple[str, ...],
) -> None:
  """A broken project — unreadable body, duplicated destinations, dangling directory — fails the turn before any backend spawn."""
  cfg = SimpleNamespace(
      **{
          **vars(make_cfg(tmp_path)), "sessions_dir": tmp_path / "sessions",
          "subprocess_buffer_limit": 1024
      })
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
  assert error_msg is not None and all(fragment in error_msg for fragment in error_fragments)
  error_events = [
      c.args[1]
      for c in callbacks.persist_and_broadcast.await_args_list  # type: ignore[attr-defined]
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
