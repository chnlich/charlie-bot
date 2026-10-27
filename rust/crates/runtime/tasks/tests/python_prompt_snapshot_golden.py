"""Prints the managed-instruction snapshots of the golden matrix as the Python server assembles them.

`prompt_snapshot_golden.rs` writes each case's temporary home and sends the case list on stdin as JSON. For each case
this helper resolves the overlay as a launch does, runs `task_prompts.build_segments` and `assemble_snapshot`, and
prints `to_json_dict()`. It then prints the `from_json_dict` error text for three damaged copies of one case. The Rust
test compares the two outputs byte for byte.
"""

from __future__ import annotations

import dataclasses
import json
import pathlib
import sys
import types

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))

from src.infra import config, models  # noqa: E402
from src.runtime import task_execution, task_prompts  # noqa: E402
from src.runtime.hooks import turn_contributions  # noqa: E402


class GoldenContribution(turn_contributions.TurnContribution):
  """The synthetic turn contribution the Rust test registers with the same answers."""

  def instruction_segments(self, meta: models.SessionMetadata, kind: str, cfg: config.CharlieBotConfig):
    if meta.name != "Scoped Contribution":
      return []
    return [
        task_prompts.RuleSegment(
            text=f"Synthetic contribution for {kind}.",
            sources=(task_prompts.PromptSource(task_prompts.SCOPE_BASE, "synthetic/contribution"),),
        )
    ]

  def workflow_rules_file(self, meta: models.SessionMetadata) -> str | None:
    return "thread_session.md" if meta.name == "Thread Brief" else None


CONTRIBUTION = GoldenContribution()


def make_meta(spec: dict) -> models.SessionMetadata:
  task = None
  if spec["task"] is not None:
    cold_read = spec["task"]["cold_read"]
    task = models.TaskSpec(
        goal=spec["task"]["goal"],
        repo_path=spec["task"]["repo_path"],
        task_type=spec["task"]["task_type"],
        cold_read=None if cold_read is None else models.ColdReadSpec(**cold_read),
    )
  return models.SessionMetadata(id=spec["id"], name=spec["name"], profile=spec["profile"], task=task)


def run_case(case: dict) -> dict:
  cfg = config.CharlieBotConfig(charliebot_home=pathlib.Path(case["home"]))
  meta = make_meta(case["meta"])
  overlay, declared = task_execution.resolve_launch_overlay(types.SimpleNamespace(prompt_overlay=case["overlay"]))
  chain = tuple((link["session_id"], link["subtree_ref"]) for link in case["chain"])
  segments, overlay_error = task_prompts.build_segments(
      cfg, meta, case["kind"], overlay=overlay, chain=chain, node_ref=case["node_ref"])
  snapshot = task_prompts.assemble_snapshot(segments)
  return {
      "name": case["name"],
      "declared": declared,
      "overlay_error": type(overlay_error).__name__ if overlay_error is not None else None,
      "snapshot": snapshot.to_json_dict(),
  }


def snapshot_of(data: dict) -> task_prompts.PromptSnapshot:
  return task_prompts.PromptSnapshot(
      blocks=tuple(
          task_prompts.PromptBlock(
              sources=tuple(task_prompts.PromptSource(**source) for source in block["sources"]),
              body_ref=block["body_ref"],
              delivery=block["delivery"],
              text=block["text"],
          ) for block in data["blocks"]))


def load_error(data: dict) -> str:
  try:
    task_prompts.PromptSnapshot.from_json_dict(data)
  except task_prompts.TaskPromptError as error:
    return str(error)
  raise AssertionError("a damaged snapshot loaded without error")


def damaged_errors(data: dict) -> dict:
  hash_damaged = {**data, "prompt_hash": "0" * 64}
  count_damaged = {**data, "char_count": data["char_count"] + 1}
  blocks = snapshot_of(data).blocks
  # The body ref changes under a recomputed hash, so only the body-ref check can fail.
  body_damaged = task_prompts.PromptSnapshot(blocks=(dataclasses.replace(blocks[0], body_ref="1" * 64),
                                                     *blocks[1:])).to_json_dict()
  return {
      "hash": load_error(hash_damaged),
      "char_count": load_error(count_damaged),
      "body_ref": load_error(body_damaged),
  }


def main() -> None:
  spec = json.load(sys.stdin)
  results = [run_case(case) for case in spec["cases"]]
  damaged = next(result["snapshot"] for result in results if result["name"] == spec["damaged_case"])
  print(json.dumps({"cases": results, "from_json_errors": damaged_errors(damaged)}, ensure_ascii=False))


if __name__ == "__main__":
  turn_contributions.register_turn_contribution("golden_synthetic", "python_prompt_snapshot_golden:CONTRIBUTION")
  main()
