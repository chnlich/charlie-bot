"""Tests for the ``steps`` cron prompt source (src/infra/config.py).

A ``steps`` cron file loads with each step's resolved body and preserved
pointer, colliding prompt sources are load errors naming the key, and duplicate
step names fail. The chain's execution shape (one leaf, ordered scheduled_step
Runs, one boundary report) is tests/test_cron_sequence.py's coverage.
"""

import pathlib

import conftest
import pytest
import yaml

from src.infra import config

# --- (a) loader --------------------------------------------------------------


def test_load_cron_file_rejects_duplicate_step_names(tmp_path: pathlib.Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = conftest.build_scheduler_cfg(tmp_path)
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
    config._load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  assert "duplicate step name 'selector'" in str(exc_info.value)


def test_load_cron_file_rejects_empty_steps(tmp_path: pathlib.Path) -> None:
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = conftest.build_scheduler_cfg(tmp_path)
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump({"cron": "0 3 * * *", "steps": []}), encoding="utf-8")
  # bool([]) is False, so an empty steps list fails the exactly-one-source check
  # before the non-empty check can name it.
  with pytest.raises(ValueError, match="task must have exactly one of"):
    config._load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")


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
    tmp_path: pathlib.Path, step_body: dict, expected_fragments: tuple[str, ...]) -> None:
  """A step carrying an inline 'prompt' — or no prompt source at all — fails the
  load, and the error names the step."""
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True)
  cfg = conftest.build_scheduler_cfg(tmp_path)
  body = {"cron": "0 3 * * *", "steps": [step_body]}
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  with pytest.raises(ValueError) as exc_info:
    config._load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  error = str(exc_info.value)
  assert all(fragment in error for fragment in expected_fragments)


# --- distinct_backend_from: the task-level model validator -------------------


def _write_chained(tmp_path: pathlib.Path, steps: list[dict], task_backend: str | None = None) -> pathlib.Path:
  """One steps cron file whose bodies live in two prompt files; returns the host path."""
  cron_dir = tmp_path / "cron.d"
  cron_dir.mkdir(parents=True, exist_ok=True)
  for i, step in enumerate(steps):
    pf = tmp_path / "prompts" / f"step{i}.md"
    pf.parent.mkdir(parents=True, exist_ok=True)
    pf.write_text(f"body {i}\n", encoding="utf-8")
    step.setdefault("prompt_file", str(pf))
  body: dict = {"cron": "0 3 * * *", "steps": steps}
  if task_backend is not None:
    body["backend"] = task_backend
  yaml_path = cron_dir / "chained.yaml"
  yaml_path.write_text(yaml.safe_dump(body, sort_keys=False), encoding="utf-8")
  return yaml_path


_DISTINCT_LOAD_REJECTION_CASES = [
    # (steps, task_backend, the ValueError fragments naming the violation).
    pytest.param(
        [{
            "name": "selector"
        }, {
            "name": "reviewer",
            "distinct_backend_from": "nope",
        }],
        None, ("step 'reviewer'", "distinct_backend_from 'nope'", "not a step of this task"),
        id="unknown-step"),
    pytest.param(
        [{
            "name": "reviewer",
            "distinct_backend_from": "selector",
        }, {
            "name": "selector"
        }],
        None, ("step 'reviewer'", "'selector'", "earlier"),
        id="later-step"),
    pytest.param(
        [{
            "name": "selector",
            "distinct_backend_from": "selector",
        }, {
            "name": "reviewer",
        }],
        None, ("step 'selector'", "earlier"),
        id="self-reference"),
    pytest.param(
        [
            {
                "name": "selector",
                "backend": "fake",
            }, {
                "name": "reviewer",
                "distinct_backend_from": "selector",
                "backend": "fake",
            }
        ],
        None, ("'reviewer'", "'selector'", "both declare backend 'fake'"),
        id="step-backends-equal"),
    pytest.param(
        [{
            "name": "selector"
        }, {
            "name": "reviewer",
            "distinct_backend_from": "selector",
        }],
        "fake", ("'reviewer'", "'selector'", "both declare backend 'fake'"),
        id="task-backends-equal"),
]


@pytest.mark.parametrize(("steps", "task_backend", "expected_fragments"), _DISTINCT_LOAD_REJECTION_CASES)
def test_load_cron_file_rejects_distinct_backend_violations(
    tmp_path: pathlib.Path, steps: list[dict], task_backend: str | None, expected_fragments: tuple[str, ...]) -> None:
  yaml_path = _write_chained(tmp_path, steps, task_backend)
  cfg = conftest.build_scheduler_cfg(tmp_path)
  with pytest.raises(ValueError) as exc_info:
    config._load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  error = str(exc_info.value)
  assert all(fragment in error for fragment in expected_fragments)


def test_load_cron_file_accepts_distinct_written_backends(tmp_path: pathlib.Path) -> None:
  """Two written, different backends load."""
  yaml_path = _write_chained(
      tmp_path, [
          {
              "name": "selector",
              "backend": "fake",
          }, {
              "name": "reviewer",
              "distinct_backend_from": "selector",
              "backend": "codex-o3",
          }
      ])
  cfg = conftest.build_scheduler_cfg(tmp_path)
  task, _mtimes = config._load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  assert task.steps[1].distinct_backend_from == "selector"


def test_load_cron_file_accepts_unset_backends_with_distinct_backend_from(tmp_path: pathlib.Path) -> None:
  """An effective backend left unset loads: the repo default names no host-local
  backend ids, and the firing-time check (src/features/cron/cron_sequence.py) covers the
  unset case by resolving what each step actually runs."""
  yaml_path = _write_chained(
      tmp_path, [{
          "name": "selector"
      }, {
          "name": "reviewer",
          "distinct_backend_from": "selector",
      }])
  cfg = conftest.build_scheduler_cfg(tmp_path)
  task, _mtimes = config._load_cron_file(yaml_path, cfg.charlie_bot_repo, "chained")
  assert task.steps[0].backend is None and task.steps[1].backend is None
