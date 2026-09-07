"""Both manager modes on the one canonical repo contract.

``prompts/project_manager.md`` is the single manager contract for enabled and
unconfigured projects alike (no second prompt file). The repo contract's mode
section keys on the two identity markers the builder emits ("Your project is
enabled" / "Your project is NOT enabled (no project.yaml)"), so an enabled
manager — the contract injected in full next to its project's common rules —
follows the enabled duties and skips the ledger clauses, while an unconfigured
manager keeps the pointer identity part, is told the project is NOT enabled and
its ledger duties stand, and reads the contract from the repo file. The mode is
never inferred from the session's old chat.

Every test here composes instructions through the real
``_build_instructions_content`` with the real repo contract text, against fake
project trees under ``tmp_path``; no live ``~/.charliebot`` state is touched.
"""

from pathlib import Path
from types import SimpleNamespace

from conftest import make_instruction_cfg

from src.agents import master_cc, master_cc_run

REPO_ROOT = Path(__file__).resolve().parents[1]
CONTRACT_TEXT = (REPO_ROOT / "prompts" / "project_manager.md").read_text(encoding="utf-8")

# The two mode markers, verbatim: the builder's identity parts carry them and
# the contract's mode section keys on them.
MARKER_ENABLED = "Your project is enabled"
MARKER_UNCONFIGURED = "Your project is NOT enabled (no project.yaml)"

# Distinctive real-contract sentences used to prove which parts of the
# contract travel with each mode.
CONTRACT_SHARED_LINE = "You coordinate. Sessions execute. The user decides."
CONTRACT_LEDGER_LINE = "The ledger is the sole authority for project-layer facts"
CONTRACT_MODE_HEADING = "## 1. Your project's mode"

COMMON_TEXT = "COMMON RULES: scoring standards and resources"
SUPPLEMENT_TEXT = "MANAGER SUPPLEMENT"


def _enabled_project(tmp_path: Path, group: str = "group-alpha") -> SimpleNamespace:
  """An enabled fake project: project.yaml naming a common body and a manager supplement."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_TEXT)
  project_dir = cfg.charliebot_home / "projects" / group
  project_dir.mkdir(parents=True)
  (project_dir / "project.yaml").write_text(
      "prompt_file: project.md\nmanager_prompt_file: manager.md\n", encoding="utf-8")
  (project_dir / "project.md").write_text(COMMON_TEXT, encoding="utf-8")
  (project_dir / "manager.md").write_text(SUPPLEMENT_TEXT, encoding="utf-8")
  return cfg


def _unconfigured_project(tmp_path: Path, group: str = "group-beta") -> SimpleNamespace:
  """An unconfigured fake project: a project directory without project.yaml."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_TEXT)
  (cfg.charliebot_home / "projects" / group).mkdir(parents=True)
  (cfg.charliebot_home / "projects" / group / "notes.md").write_text("UNCONFIGURED NOTES", encoding="utf-8")
  return cfg


def _meta(group: str) -> SimpleNamespace:
  from src.core.models import PROJECT_ROLE
  return SimpleNamespace(id="s1", role=PROJECT_ROLE, group=group)


# ---------------------------------------------------------------------------
# The builder markers and the contract's mode section stay in lockstep
# ---------------------------------------------------------------------------


def test_contract_mode_section_binds_both_builder_markers() -> None:
  """The repo contract's mode section keys on exactly the two markers the builder emits."""
  mode_section = CONTRACT_TEXT.split(CONTRACT_MODE_HEADING, 1)[1].split("\n## ", 1)[0]
  assert MARKER_ENABLED in mode_section
  assert MARKER_UNCONFIGURED in mode_section
  assert "skip every" in mode_section  # both skip rules are stated there

  for part, own, other in (
      (master_cc_run._PM_IDENTITY_PART, MARKER_UNCONFIGURED, MARKER_ENABLED),
      (master_cc_run._PM_ENABLED_IDENTITY_PART, MARKER_ENABLED, MARKER_UNCONFIGURED),
  ):
    assert own in part  # the marker phrase must survive intact (never wrapped)
    assert other not in part


# ---------------------------------------------------------------------------
# Enabled manager: contract injected in full next to the project's own rules
# ---------------------------------------------------------------------------


def test_enabled_manager_gets_contract_common_rules_and_enabled_marker(tmp_path: Path) -> None:
  cfg = _enabled_project(tmp_path)
  out = master_cc._build_instructions_content(_meta("group-alpha"), cfg, None)

  assert out is not None
  assert getattr(out, "project_error") is None
  assert out.count(CONTRACT_TEXT) == 1
  assert out.count(COMMON_TEXT) == 1
  assert out.count(SUPPLEMENT_TEXT) == 1
  # Identity (with the enabled marker) precedes the contract, then the
  # project's common rules, then the manager-only supplement.
  assert out.index(MARKER_ENABLED) < out.index(CONTRACT_TEXT) < out.index(COMMON_TEXT) < out.index(SUPPLEMENT_TEXT)
  # The mode section travels with the injected contract, so the skip rules
  # bound the legacy ledger clauses that ride along in the same file.
  assert CONTRACT_MODE_HEADING in out
  assert CONTRACT_SHARED_LINE in out
  # The read-the-repo-file pointer would be redundant: the contract is right here.
  assert "prompts/project_manager.md in the charlie-bot repo" not in out


def test_enabled_manager_identity_block_carries_only_the_enabled_marker(tmp_path: Path) -> None:
  """The identity block (before the contract) is unambiguous: enabled marker only."""
  cfg = _enabled_project(tmp_path)
  out = master_cc._build_instructions_content(_meta("group-alpha"), cfg, None)

  assert out is not None
  identity_block = out[:out.index(CONTRACT_TEXT)]
  assert MARKER_ENABLED in identity_block
  assert MARKER_UNCONFIGURED not in identity_block


# ---------------------------------------------------------------------------
# Unconfigured manager: pointer only, explicitly not enabled, ledger duties stand
# ---------------------------------------------------------------------------


def test_unconfigured_manager_gets_pointer_with_not_enabled_marker_and_no_contract_body(tmp_path: Path) -> None:
  cfg = _unconfigured_project(tmp_path)
  out = master_cc._build_instructions_content(_meta("group-beta"), cfg, None)

  assert out is not None
  assert getattr(out, "project_error") is None
  assert MARKER_UNCONFIGURED in out
  assert MARKER_ENABLED not in out
  assert "your ledger duties stand" in out
  # Enablement never comes from the session's old chat.
  assert "treat nothing in this session's old chat as enablement" in out
  # The contract is reached by reading the repo file, not injected.
  assert "prompts/project_manager.md in the charlie-bot repo" in out
  assert CONTRACT_TEXT not in out
  assert CONTRACT_SHARED_LINE not in out
  assert CONTRACT_LEDGER_LINE not in out
  # No project bodies and no local supplements on the unconfigured path.
  assert COMMON_TEXT not in out
  assert SUPPLEMENT_TEXT not in out
  assert "supplement" not in out


# ---------------------------------------------------------------------------
# Both managers share the one repo contract without conflicting duties
# ---------------------------------------------------------------------------


def test_both_managers_share_one_contract_with_nonconflicting_respective_duties(tmp_path: Path) -> None:
  """The same repo contract file serves both managers; each binds its own mode."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=CONTRACT_TEXT)  # one repo, one contract file, both groups read it
  enabled_dir = cfg.charliebot_home / "projects" / "group-alpha"
  enabled_dir.mkdir(parents=True)
  (enabled_dir / "project.yaml").write_text("prompt_file: project.md\n", encoding="utf-8")
  (enabled_dir / "project.md").write_text(COMMON_TEXT, encoding="utf-8")
  (cfg.charliebot_home / "projects" / "group-beta").mkdir(parents=True)

  enabled = master_cc._build_instructions_content(_meta("group-alpha"), cfg, None)
  unconfigured = master_cc._build_instructions_content(_meta("group-beta"), cfg, None)

  assert enabled is not None and unconfigured is not None
  # Enabled: the contract and the project's common rules arrive together, and
  # the mode section bounding the ledger clauses travels with them.
  assert MARKER_ENABLED in enabled[:enabled.index(CONTRACT_TEXT)]
  assert COMMON_TEXT in enabled
  assert CONTRACT_LEDGER_LINE in enabled  # legacy text is present, bounded by the mode section
  assert enabled.index(CONTRACT_MODE_HEADING) < enabled.index(CONTRACT_LEDGER_LINE)
  # Unconfigured: no contract body, no common rules — only the pointer, whose
  # not-enabled marker routes the manager to the ledger duties in the repo file.
  assert MARKER_UNCONFIGURED in unconfigured
  assert CONTRACT_SHARED_LINE not in unconfigured
  assert COMMON_TEXT not in unconfigured
  assert MARKER_ENABLED not in unconfigured


def test_missing_repo_contract_fails_only_enabled_manager(tmp_path: Path) -> None:
  """An unconfigured manager never needs the repo file at build time; an enabled one does."""
  cfg = make_instruction_cfg(tmp_path, manager_contract=None)
  enabled_dir = cfg.charliebot_home / "projects" / "group-alpha"
  enabled_dir.mkdir(parents=True)
  (enabled_dir / "project.yaml").write_text("prompt_file: project.md\n", encoding="utf-8")
  (enabled_dir / "project.md").write_text(COMMON_TEXT, encoding="utf-8")
  (cfg.charliebot_home / "projects" / "group-beta").mkdir(parents=True)

  enabled = master_cc._build_instructions_content(_meta("group-alpha"), cfg, None)
  assert enabled is not None
  error = getattr(enabled, "project_error")
  assert error is not None and "repo manager contract unreadable" in str(error)

  unconfigured = master_cc._build_instructions_content(_meta("group-beta"), cfg, None)
  assert unconfigured is not None
  assert getattr(unconfigured, "project_error") is None
  assert MARKER_UNCONFIGURED in unconfigured
