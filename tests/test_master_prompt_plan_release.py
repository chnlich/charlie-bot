"""Pin the plan-release contract wording in its current homes.

The plan-registration block used to be quoted verbatim from prompts/master.md.
The contract moved: master.md now routes plan approval through
skills/plan-approval/SKILL.md, which carries the registration, approval, and
verify-step wording, while the verifier's read-only network envelope lives in
the verify worker prompt (prompts/verify.md). These tests pin the current
wording so contract changes stay deliberate.
"""

from pathlib import Path

from src.core import spawner

EXPECTED_PLAN_REGISTRATION_BULLET = (
    "- Register before presenting via `charliebot plan present` (verbs per `charliebot plan --help`). The"
    " artifact's status chip is a presentation-time snapshot; the plan registry is the live truth. Record the"
    " code baseline when the plan pins one."
)

_PLAN_APPROVAL_SKILL_PATH = "skills/plan-approval/SKILL.md"


def _repo_root() -> Path:
  return Path(spawner.__file__).resolve().parents[2]


def _read_repo_file(relative: str) -> str:
  return (_repo_root() / relative).read_text(encoding="utf-8")


def _master_prompt() -> str:
  return _read_repo_file("prompts/master.md")


def _plan_approval_skill() -> str:
  return _read_repo_file(_PLAN_APPROVAL_SKILL_PATH)


def _skill_section(heading: str) -> str:
  skill = _plan_approval_skill()
  start = skill.index(f"## {heading}\n")
  next_heading = skill.find("\n## ", start)
  return skill[start:] if next_heading == -1 else skill[start:next_heading]


def _normalized(text: str) -> str:
  return " ".join(text.split())


def test_plan_registration_block_matches_approved_literal_and_format() -> None:
  raw_plan_section = _skill_section("Plan")

  assert raw_plan_section.splitlines().count(EXPECTED_PLAN_REGISTRATION_BULLET) == 1


def test_plan_registration_says_verify_is_required_and_independent() -> None:
  approval = _normalized(_skill_section("Approval"))
  verify = _normalized(_skill_section("Verify"))
  skill = _normalized(_plan_approval_skill())

  assert "Verify is a required independent step" in approval
  assert "its results live in the verify thread's own log, never in the plan registry" in approval
  assert "Verify is a read-only repo-less delegation and runs on a registered plan" in verify
  assert "Verify is optional" not in skill


def test_plan_registration_includes_present_registration_step() -> None:
  plan = _normalized(_skill_section("Plan"))
  skill = _normalized(_plan_approval_skill())

  assert "Register before presenting via `charliebot plan present` (verbs per `charliebot plan --help`)" in plan
  assert "--verify-thread" not in skill


def test_plan_registration_states_chip_is_snapshot_and_registry_is_live_truth() -> None:
  plan = _normalized(_skill_section("Plan"))

  assert ("The artifact's status chip is a presentation-time snapshot; the plan registry is the live truth."
          in plan)


def test_plan_registration_includes_unconditional_approve_after_takeoff() -> None:
  approval = _normalized(_skill_section("Approval"))

  assert "After the user says take off, record it via `charliebot plan approve`" in approval
  assert "`approve` records a takeoff unconditionally" in approval
  assert "approve bookkeeping does not gate execution" in approval


def test_plan_registration_closes_superseded_lineage_and_no_reverify() -> None:
  feedback = _normalized(_skill_section("Feedback"))
  skill = _normalized(_plan_approval_skill())

  assert "`charliebot plan present` opens the new lineage and the old one closes as superseded" in feedback
  assert "reverify" not in skill


def test_verify_worker_prompt_documents_read_only_network_contract() -> None:
  preamble = _read_repo_file("prompts/verify.md")

  assert "You are a read-only plan verifier." in preamble
  assert "Retrieve evidence through allowed local and network reads, and report findings." in preamble
  assert ("Refuse and report any requested part that would mutate local or external state instead of executing it."
          in preamble)
  assert "the boundary is semantic read-only behavior, not a transport or HTTP-method allowlist" in preamble


def test_takeoff_confirmation_routes_improve_through_plan_registry() -> None:
  plan = _normalized(_skill_section("Plan"))
  master_prompt = _normalized(_master_prompt())

  assert "An improve-loop takeoff plan follows this same contract" in plan
  assert "Take-off follows `skills/plan-approval/SKILL.md`" in master_prompt
