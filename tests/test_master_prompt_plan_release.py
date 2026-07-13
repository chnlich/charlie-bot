from pathlib import Path

from src.core import spawner


def test_plan_release_requires_verifier_creation_before_in_flight_presentation() -> None:
  repo_root = Path(spawner.__file__).resolve().parents[2]
  prompt = (repo_root / "prompts" / "master.md").read_text(encoding="utf-8")
  release_start = prompt.index("Every plan uses one release sequence")
  completion_start = prompt.index("Verifier completion wakes the master")
  release_contract = prompt[release_start:completion_start]

  write_index = release_contract.index("write the plan artifact and its structured verify task spec")
  launch_index = release_contract.index("launch the `verify` delegation")
  identifier_index = release_contract.index("non-empty `thread_id` worker identifier")
  presentation_index = release_contract.index("only then present the plan with `verification · in flight`")
  assert write_index < launch_index < identifier_index < presentation_index
  assert "worker identifier proves verifier creation, not verifier completion" in release_contract
  assert "do not present the plan" in release_contract
  assert "do not claim verification has started" in release_contract


def test_master_prompt_documents_verify_read_only_network_contract() -> None:
  repo_root = Path(spawner.__file__).resolve().parents[2]
  prompt = (repo_root / "prompts" / "master.md").read_text(encoding="utf-8")
  profile_start = prompt.index("- `verify` —")
  profile_end = prompt.index("\n\n- **Always delegate**", profile_start)
  verify_profile = prompt[profile_start:profile_end]

  assert "read-only local and network access through existing capabilities" in verify_profile
  assert "refuses local or external state mutation" in verify_profile


def test_master_prompt_routes_each_plan_anchor_type() -> None:
  repo_root = Path(spawner.__file__).resolve().parents[2]
  prompt = (repo_root / "prompts" / "master.md").read_text(encoding="utf-8")
  checklists_start = prompt.index("Derive the verifier task spec")
  completion_start = prompt.index("Verifier completion wakes the master", checklists_start)
  verifier_checklists = prompt[checklists_start:completion_start]

  assert "code facts use absolute paths on the main checkout" in verifier_checklists
  assert "external material uses URL anchors checked through read-only network access" in verifier_checklists
  assert "runtime facts use reproducible read-only commands" in verifier_checklists
