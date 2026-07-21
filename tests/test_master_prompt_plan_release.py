from pathlib import Path

from src.core import spawner

EXPECTED_PLAN_REGISTRATION_BLOCK = """### Plan registration

- Before presentation, write the HTML artifact. Optionally launch a read-only `verify` worker to reality-check
  the plan; if you do, read its result from the verify thread's own log. Verify is optional and independent;
  its outcome never enters the plan registry.
- Register the plan with `charliebot plan present --file <artifacts/plan_NN.html> --title <…>` (revisions use
  `plan amend`, with `--plan` when ambiguous). The artifact's status chip is a presentation-time snapshot; the
  live truth is the plan registry. Record the code baseline with `--base-repo/--base-branch/--base-sha` when
  the plan pins one.
- After the user says `take off`, run `charliebot plan approve [--plan N]` to record it. Approval is
  unconditional — `approve` does not read any verify state.
- Abandoned or superseded directions run `charliebot plan close --plan N --as superseded|abandoned`."""


def _master_prompt() -> str:
  repo_root = Path(spawner.__file__).resolve().parents[2]
  return (repo_root / "prompts" / "master.md").read_text(encoding="utf-8")


def _plan_registration_block() -> str:
  prompt = _master_prompt()
  start = prompt.index("### Plan registration")
  end = prompt.index("\n\nAim for well-organized", start)
  return prompt[start:end]


def _normalized_plan_registration_block() -> str:
  return " ".join(_plan_registration_block().split())


def test_plan_registration_block_matches_approved_literal_and_format() -> None:
  block = _plan_registration_block()

  assert block == EXPECTED_PLAN_REGISTRATION_BLOCK
  assert max(len(line) for line in block.splitlines()) <= 120


def test_plan_registration_says_verify_is_optional_and_independent() -> None:
  block = _normalized_plan_registration_block()

  assert "Optionally launch a read-only `verify` worker" in block
  assert "Verify is optional and independent" in block
  assert "its outcome never enters the plan registry" in block


def test_plan_registration_includes_present_registration_step() -> None:
  block = _normalized_plan_registration_block()

  assert "charliebot plan present --file <artifacts/plan_NN.html> --title <…>" in block
  assert "revisions use `plan amend`, with `--plan` when ambiguous" in block
  assert "--verify-thread" not in block


def test_plan_registration_states_chip_is_snapshot_and_registry_is_live_truth() -> None:
  block = _normalized_plan_registration_block()

  assert "The artifact's status chip is a presentation-time snapshot" in block
  assert "the live truth is the plan registry" in block


def test_plan_registration_includes_unconditional_approve_after_takeoff() -> None:
  block = _normalized_plan_registration_block()

  assert "charliebot plan approve [--plan N]" in block
  assert "Approval is unconditional" in block
  assert "report a rejection detail verbatim" not in block


def test_plan_registration_includes_close_and_no_reverify() -> None:
  block = _normalized_plan_registration_block()

  assert "charliebot plan close --plan N --as superseded|abandoned" in block
  assert "reverify" not in block


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
  assert "see Plan registration" in block
