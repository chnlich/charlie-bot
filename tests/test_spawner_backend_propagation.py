from pathlib import Path

import pytest
from conftest import (
    AGY_BACKEND_OPTION,
    CODEX_BACKEND_OPTION,
    OPUS_BACKEND_ID,
    backend_option,
    build_worker_prompt,
)

from src.infra.config import CharlieBotConfig
from src.infra.models import TaskType
from src.runtime import spawner


def _build_cfg() -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options":
              [
                  backend_option(
                      id=OPUS_BACKEND_ID,
                      label="Opus",
                      type="cc-claude",
                      model="claude-opus-4-6",
                      effort="max",
                      cli_binary="claude-sub",
                  ),
                  CODEX_BACKEND_OPTION,
              ]
      },
  )


def test_resolve_backend_option_requires_valid_backend_and_model() -> None:
  cfg = _build_cfg()
  opt = spawner.spawner_backends.resolve_backend_option(cfg, OPUS_BACKEND_ID, "claude-opus-4-6")
  assert opt.id == OPUS_BACKEND_ID
  assert opt.model == "claude-opus-4-6"
  assert opt.effort == "max"
  assert opt.cli_binary == "claude-sub"

  with pytest.raises(ValueError, match=r"is not in backends.options"):
    spawner.spawner_backends.resolve_backend_option(cfg, "missing", "o3")

  with pytest.raises(ValueError, match="model is required"):
    spawner.spawner_backends.resolve_backend_option(cfg, "codex-o3", "")


def test_resolve_backend_option_allows_antigravity_missing_model() -> None:
  cfg = CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={"options": [AGY_BACKEND_OPTION,]},
  )

  opt = spawner.spawner_backends.resolve_backend_option(cfg, "agy", None)

  assert opt.id == "agy"
  assert opt.model is None


@pytest.mark.parametrize(
    "backend_type, entry_kwargs",
    [
        ("cc-claude", {}),
        ("cc-kimi", {
            "credential": "test-kimi"
        }),
        ("cc-openai-compatible", {
            "api_base": "https://api.test/v1"
        }),
        ("codex", {}),
        ("charlie-code", {}),
        ("gemini", {}),
        ("opencode", {}),
    ],
)
def test_resolve_backend_option_rejects_missing_model_for_model_required_backends(
    backend_type: str, entry_kwargs: dict) -> None:
  # The sectioned schema rejects a model-less model-required entry outright, so
  # each config entry carries a model and the resolver's own None rejection is
  # what the test drives.
  cfg = CharlieBotConfig(
      charliebot_home=Path("/tmp/charliebot-test"),
      paths={"worktree_dir": "/tmp/worktrees"},
      backends={
          "options":
              [
                  backend_option(
                      id=backend_type, label=backend_type, type=backend_type, model="fake-model", **entry_kwargs),
              ]
      },
  )

  with pytest.raises(ValueError, match="model is required"):
    spawner.spawner_backends.resolve_backend_option(cfg, backend_type, None)


def test_build_worker_prompt_makes_iteration_reports_advisory() -> None:
  prompt = build_worker_prompt("Improve the CLI", cfg=_build_cfg(), loop_dir="/tmp/loops/2", iteration_number=2)

  assert "Treat them as advisory evidence and hints only." in prompt
  assert "must not dictate your plan for this iteration" not in prompt
  assert "### What Changed" in prompt
  assert "### Evidence" in prompt
  assert "### Advisory Notes" in prompt
  assert "### Next" not in prompt


def test_build_worker_prompt_task_type_implement_matches_legacy_format() -> None:
  prompt = build_worker_prompt("Implement X", cfg=_build_cfg())
  assert "commits-and-prs.md" in prompt
  assert "A reviewer will handle that." in prompt
  assert "Do NOT modify tracked files." not in prompt
  assert "Do NOT commit." not in prompt


def test_build_worker_prompt_instructs_task_spec_source_file_handling() -> None:
  prompt = build_worker_prompt("## Goal\nImplement X\n\n## Source Files\n- /tmp/source.md", cfg=_build_cfg())

  assert "contains a `## Source Files` section" in prompt
  assert "read every listed source file before editing" in prompt
  assert "stop and report the conflict" in prompt
  assert "instead of inventing a merged requirement" in prompt


def test_build_worker_prompt_task_type_quick_edit_skips_reviewer_mention() -> None:
  prompt = build_worker_prompt("Cherry-pick fix", cfg=_build_cfg(), task_type=TaskType.QUICK_EDIT)
  assert "commits-and-prs.md" in prompt
  assert "No reviewer will run" in prompt
  assert "A reviewer will handle that." not in prompt


def test_build_worker_prompt_task_type_script_run_forbids_edits_and_commits() -> None:
  prompt = build_worker_prompt("Run SLURM benchmark", cfg=_build_cfg(), task_type=TaskType.SCRIPT_RUN)
  assert "Do NOT modify tracked files" in prompt
  assert "Do NOT commit" in prompt
  assert "commits-and-prs.md" not in prompt
  assert "A reviewer will handle that." not in prompt


def test_build_worker_prompt_rejects_verify_task_type() -> None:
  with pytest.raises(ValueError, match="unsupported task_type"):
    build_worker_prompt("Verify plan", cfg=_build_cfg(), task_type=TaskType.VERIFY)
