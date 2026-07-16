from pathlib import Path

from src.core import spawner

EXPECTED_PLAN_VERIFICATION_BLOCK = """### Plan verification

- Before presentation, write the HTML and verifier spec, launch a read-only `verify` worker, and require a
  non-empty `thread_id`; otherwise report the launch error and withhold the plan. Present successful launches as
  `verification · in flight`.
- Verifier specs must not define or restate the report format; the harness verify prompt is the single authority for it.
- First versions get a full check of anchored current reality, approval-object completeness and placement, and
  standalone readability. Amendments check only changed approval terms, their dependent claims, prior mismatches,
  and document structure. The verifier spec declares which mode applies. Check code through absolute paths pinned
  to the plan's commit, external evidence through URL anchors and read-only network access, and runtime facts
  through reproducible read-only commands. Check branch drift once before implementation.
- A plan lineage's verification status is its latest verify run: after amending a plan that had
  mismatches, launch the delta re-verify before presenting the amended plan.
- Verifiers cannot add approval terms. A completeness mismatch is a design decision or irreversible action absent
  from the approval surface; implementation detail is never a missing term. Leave ambiguous evidence as an open
  Trade-off. Auto-amend once per plan lineage (one presented plan plus its automatic amendments; new user feedback
  starts a new lineage); a second approval mismatch returns control to the user.
- A clean result updates the chip and releases a recorded `take off`. Approval mismatches require a fresh
  `take off`; other findings may be amended without reapproval."""


def _master_prompt() -> str:
  repo_root = Path(spawner.__file__).resolve().parents[2]
  return (repo_root / "prompts" / "master.md").read_text(encoding="utf-8")


def _plan_verification_block() -> str:
  prompt = _master_prompt()
  start = prompt.index("### Plan verification")
  end = prompt.index("\n\nAim for well-organized", start)
  return prompt[start:end]


def _normalized_plan_verification_block() -> str:
  return " ".join(_plan_verification_block().split())


def test_plan_verification_block_matches_approved_literal_and_format() -> None:
  block = _plan_verification_block()

  assert block == EXPECTED_PLAN_VERIFICATION_BLOCK
  assert len(block.split()) == 248
  assert max(len(line) for line in block.splitlines()) <= 120


def test_plan_release_requires_verifier_creation_before_in_flight_presentation() -> None:
  block = _normalized_plan_verification_block()

  write_index = block.index("write the HTML and verifier spec")
  launch_index = block.index("launch a read-only `verify` worker")
  identifier_index = block.index("non-empty `thread_id`")
  presentation_index = block.index("Present successful launches as")
  assert write_index < launch_index < identifier_index < presentation_index
  assert "otherwise report the launch error and withhold the plan" in block
  assert "`verification · in flight`" in block


def test_plan_verifier_scope_is_spec_declared_full_or_delta() -> None:
  block = _normalized_plan_verification_block()

  assert "First versions get a full check" in block
  assert "Amendments check only changed approval terms, their dependent claims, prior mismatches" in block
  assert "The verifier spec declares which mode applies" in block


def test_master_prompt_routes_each_plan_anchor_type() -> None:
  block = _normalized_plan_verification_block()

  assert "code through absolute paths pinned to the plan's commit" in block
  assert "external evidence through URL anchors and read-only network access" in block
  assert "runtime facts through reproducible read-only commands" in block


def test_plan_verification_defines_completeness_and_auto_amend_lineage() -> None:
  block = _normalized_plan_verification_block()

  assert "A completeness mismatch is a design decision or irreversible action absent from the approval surface" in block
  assert "implementation detail is never a missing term" in block
  assert "one presented plan plus its automatic amendments; new user feedback starts a new lineage" in block
  assert "Auto-amend once per plan lineage" in block
  assert "a second approval mismatch returns control to the user" in block


def test_plan_verification_records_and_releases_takeoff_by_finding_type() -> None:
  block = _normalized_plan_verification_block()

  assert "A clean result updates the chip and releases a recorded `take off`" in block
  assert "Approval mismatches require a fresh `take off`" in block
  assert "other findings may be amended without reapproval" in block


def test_master_prompt_documents_verify_read_only_network_contract() -> None:
  prompt = _master_prompt()
  profile_start = prompt.index("- `verify` —")
  profile_end = prompt.index("\n\n- **Always delegate**", profile_start)
  verify_profile = prompt[profile_start:profile_end]

  assert "read-only local and network access through existing capabilities" in verify_profile
  assert "refuses local or external state mutation" in verify_profile
