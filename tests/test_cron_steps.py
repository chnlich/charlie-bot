"""Tests for the ``steps`` cron prompt source (src/core/config.py).

A ``steps`` cron file loads with each step's resolved body and preserved
pointer, colliding prompt sources are load errors naming the key, and duplicate
step names fail. The chain's execution shape (one leaf, ordered scheduled_step
Runs, one boundary report) is tests/test_cron_sequence.py's coverage.
"""

from pathlib import Path

import pytest
import yaml
from conftest import build_scheduler_cfg

from src.core.config import _load_cron_file

SELECTOR_BODY = "Select memory candidates.\n"
REVIEWER_BODY = "Review the memory diff.\n"
SELECTOR_RESULT = "revise entries/workflow/focus.md plus three proof lines"
REVIEWER_RESULT = "report written to the session artifacts"

# --- (a) loader --------------------------------------------------------------


def test_load_cron_file_rejects_duplicate_step_names(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = build_scheduler_cfg(tmp_path)
  sel_path = tmp_path / "prompts" / "selector.md"
  sel_path.parent.mkdir(parents=True, exist_ok=True)
  sel_path.write_text("Select.\n", encoding="utf-8")
  body = {
      "cron":
          "0 3 * * *",
      "steps": [
          {
              "name": "selector",
              "prompt_file": str(sel_path)
          },
          {
              "name": "selector",
              "prompt_file": str(sel_path)
          },
      ],
  }
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  with pytest.raises(ValueError) as exc_info:
    _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  assert "duplicate step name 'selector'" in str(exc_info.value)


def test_load_cron_file_rejects_empty_steps(tmp_path: Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = build_scheduler_cfg(tmp_path)
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump({"cron": "0 3 * * *", "steps": []}), encoding="utf-8")
  # bool([]) is False, so an empty steps list fails the exactly-one-source check
  # before the non-empty check can name it.
  with pytest.raises(ValueError, match="task must have exactly one of"):
    _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")


_STEP_PROMPT_SOURCE_CASES = [
    # (one steps entry, the ValueError fragments naming the step-level violation).
    pytest.param({
        "name": "selector",
        "prompt": "inline body"
    }, ("inline 'prompt'", "step"), id="inline-prompt"),
    pytest.param({"name": "selector"}, ("step 'selector'",), id="missing-prompt-body"),
]


@pytest.mark.parametrize(("step_body", "expected_fragments"), _STEP_PROMPT_SOURCE_CASES)
def test_load_cron_file_rejects_step_prompt_source_violation(
    tmp_path: Path, step_body: dict, expected_fragments: tuple[str, ...]) -> None:
  """A step carrying an inline 'prompt' — or no prompt source at all — fails the
  load, and the error names the step."""
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = build_scheduler_cfg(tmp_path)
  body = {"cron": "0 3 * * *", "steps": [step_body]}
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  with pytest.raises(ValueError) as exc_info:
    _load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  error = str(exc_info.value)
  assert all(fragment in error for fragment in expected_fragments)
