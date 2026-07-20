from pathlib import Path

from src.core import spawner

EXPECTED_PLAN_VERIFICATION_BLOCK = """### Plan verification

- Before presentation, write the HTML and verifier spec, launch a read-only `verify` worker, and require a
  non-empty `thread_id`; otherwise report the launch error and withhold the plan. After the launch, register the
  plan with `charliebot plan present --file <artifacts/plan_NN.html> --verify-thread <id> --title <…>` (revisions
  use `plan amend`, with `--plan` when ambiguous); the artifact's verification chip is a presentation-time
  snapshot, and the live truth is the plan registry. Present successful launches as `verification · in flight`.
- Verifier specs must not define or restate the report format; the harness verify prompt is the single authority for it.
- Verification checks current reality of the approval object only. Every factual claim in an approval-object
  term must carry a checkable anchor; an unanchored factual claim is a mismatch. Check each anchored claim
  against its anchor — code through absolute paths pinned to the plan's commit, external evidence through URL
  anchors and read-only network access, runtime facts through reproducible read-only commands; an anchor that
  does not support its claim is a mismatch. Document quality — readability, completeness, placement, structure —
  is out of verification scope. Verifiers cannot add approval terms; leave ambiguous evidence as an open
  Trade-off. Check branch drift once before implementation.
- Verification binds to the approval object, not the document version: an amendment that leaves approval terms
  unchanged never re-verifies. Re-verify in delta mode — only the changed terms and their dependent claims — when
  an approval mismatch is amended or user feedback changes approval terms. The verifier spec declares full or
  delta mode.
- Non-approval mismatches never block: fix them in the artifact once, note the fix in revnotes, do not re-verify.
  A recorded `take off` releases when the run completes with no approval mismatch. An approval mismatch requires
  amendment, a delta re-verify, and a fresh `take off`; a second approval mismatch in one lineage (one presented
  plan plus its amendments; new user feedback starts a new lineage) returns control to the user. After consuming
  the user's take off, run `charliebot plan approve [--plan N]`; report a rejection detail verbatim instead of
  proceeding. Abandoned or superseded directions run `charliebot plan close --plan N --as superseded|abandoned`.
  Operational verifier failures run `charliebot plan reverify --verify-thread <new-id> [--plan N]`."""


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
  assert len(block.split()) == 370
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


def test_plan_verification_includes_present_registration_step() -> None:
  block = _normalized_plan_verification_block()

  assert "charliebot plan present --file <artifacts/plan_NN.html> --verify-thread <id> --title <…>" in block
  assert "revisions use `plan amend`, with `--plan` when ambiguous" in block


def test_plan_verification_states_chip_is_snapshot_and_registry_is_live_truth() -> None:
  block = _normalized_plan_verification_block()

  assert "the artifact's verification chip is a presentation-time snapshot" in block
  assert "the live truth is the plan registry" in block


def test_plan_verification_includes_approve_after_takeoff_with_verbatim_rejection() -> None:
  block = _normalized_plan_verification_block()

  assert "charliebot plan approve [--plan N]" in block
  assert "report a rejection detail verbatim instead of proceeding" in block


def test_plan_verification_includes_close_and_reverify() -> None:
  block = _normalized_plan_verification_block()

  assert "charliebot plan close --plan N --as superseded|abandoned" in block
  assert "charliebot plan reverify --verify-thread <new-id> [--plan N]" in block
  assert "Operational verifier failures run" in block
  assert "Abandoned or superseded directions run" in block


def test_plan_verifier_scope_is_spec_declared_full_or_delta() -> None:
  block = _normalized_plan_verification_block()

  assert "The verifier spec declares full or delta mode" in block
  assert "Re-verify in delta mode" in block


def test_master_prompt_routes_each_plan_anchor_type() -> None:
  block = _normalized_plan_verification_block()

  assert "code through absolute paths pinned to the plan's commit" in block
  assert "external evidence through URL anchors and read-only network access" in block
  assert "runtime facts through reproducible read-only commands" in block


def test_plan_verification_defines_completeness_and_auto_amend_lineage() -> None:
  block = _normalized_plan_verification_block()

  assert "Verifiers cannot add approval terms" in block
  assert "one presented plan plus its amendments; new user feedback starts a new lineage" in block
  assert "a second approval mismatch in one lineage" in block
  assert "returns control to the user" in block


def test_plan_verification_records_and_releases_takeoff_by_finding_type() -> None:
  block = _normalized_plan_verification_block()

  assert "A recorded `take off` releases when the run completes with no approval mismatch" in block
  assert "An approval mismatch requires amendment, a delta re-verify, and a fresh `take off`" in block


def test_master_prompt_documents_verify_read_only_network_contract() -> None:
  prompt = _master_prompt()
  profile_start = prompt.index("- `verify` —")
  profile_end = prompt.index("\n\n- **Always delegate**", profile_start)
  verify_profile = prompt[profile_start:profile_end]

  assert "read-only local and network access through existing capabilities" in verify_profile
  assert "refuses local or external state mutation" in verify_profile


def test_takeoff_confirmation_routes_improve_through_plan_registry() -> None:
  prompt = _master_prompt()
  start = prompt.index("### Take-off confirmation")
  end = prompt.index("\n\n---", start)
  block = " ".join(prompt[start:end].split())

  assert "Improve-loop takeoff plans go through the same plan registry registration" in block
  assert "see Plan verification" in block
