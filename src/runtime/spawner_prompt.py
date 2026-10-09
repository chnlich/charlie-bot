"""Worker/verify prompt support — template loading and marker-section extraction."""

import pathlib

from src.infra import config, models
from src.runtime import verify_trailer

_PROMPT_SECTION_MARKER_PREFIX = "<!-- section: "
_PROMPT_SECTION_MARKER_SUFFIX = " -->"

# Single home of the per-task-type worker.md section selection. Element 0 is
# the bindings section rendered per run; elements 1: form the persistent
# workflow rule. A task type absent
# from the map fails loud rather than selecting a wrong workflow body.
# implement and quick_edit compose the template's shared workflow_steps body
# with their own closing STOP section, so the commit-message rule lives once
# in prompts/worker.md; script_run's body is one complete section.
# _REQUIRED_WORKER_PROMPT_SECTIONS derives its id set from this map.
WORKFLOW_PROMPT_SECTION = {
    models.TaskType.IMPLEMENT: ("worktree_bindings", "workflow_steps", "workflow_implement"),
    models.TaskType.QUICK_EDIT: ("worktree_bindings", "workflow_steps", "workflow_quick_edit"),
    models.TaskType.SCRIPT_RUN: ("workflow_script_run_bindings", "workflow_script_run"),
}

# The repo-less delegation's section selection (no repo: the Run directory is
# the working directory). implement and quick_edit share the one repo-less
# workflow body; script_run keeps its sandbox contract, whose sandbox is the
# Run directory. Each entry rides the repo-less source-files rule, which names
# host paths instead of a checkout.
REPO_LESS_WORKFLOW_SECTION = {
    models.TaskType.IMPLEMENT: ("workflow_repo_less",),
    models.TaskType.QUICK_EDIT: ("workflow_repo_less",),
    models.TaskType.SCRIPT_RUN: ("workflow_script_run",),
}
REPO_LESS_SOURCE_FILES_SECTION = "task_spec_source_files_repo_less"


def workflow_rule_section_ids(task_type: models.TaskType, *, repo_less: bool) -> tuple[str, ...]:
  """The persistent workflow rule sections of one work Run's task type.

  The repo case reads WORKFLOW_PROMPT_SECTION from element 1: element 0 is the
  bindings section, which renders per-run and is never a persistent rule. The
  repo-less case selects REPO_LESS_WORKFLOW_SECTION with its own source-files
  rule; a task type absent from that map falls back to the repo contract.
  """
  if repo_less:
    ids = REPO_LESS_WORKFLOW_SECTION.get(task_type)
    if ids is not None:
      return (*ids, REPO_LESS_SOURCE_FILES_SECTION)
  return (*WORKFLOW_PROMPT_SECTION[task_type][1:], "task_spec_source_files")


_REQUIRED_WORKER_PROMPT_SECTIONS = (
    "session_info",
    "coding_principles",
    "skills_discovery",
    "remote_scratch",
    "role",
    "intro_new",
    "intro_continuation",
    *dict.fromkeys(sid for ids in WORKFLOW_PROMPT_SECTION.values() for sid in ids),
    *dict.fromkeys(sid for ids in REPO_LESS_WORKFLOW_SECTION.values() for sid in ids),
    REPO_LESS_SOURCE_FILES_SECTION,
    "task_spec_source_files",
    "task",
    "iteration_reports",
    "worktree_persistence",
    "memory",
)


def _load_prompt_sections(path: pathlib.Path, required: tuple[str, ...], *, extraction: str) -> dict[str, str]:
  """Read a marker-sectioned prompt template fresh and return its sections.

  Sections are split on the template's `<!-- section: <id> -->` marker lines.
  Stateless and uncached: every call re-reads the file so an edit takes effect on the
  next spawn. Missing file or missing required section raises with the file's full
  path and the most likely cause -- the repo checkout predates the *extraction*
  commit that moved this prompt out of Python. No embedded-text fallback.
  """
  if not path.is_file():
    raise FileNotFoundError(
        f"prompt template not found at {path} — the repo checkout most likely "
        f"predates the {extraction} extraction commit")
  sections: dict[str, str] = {}
  current_id: str | None = None
  current_lines: list[str] = []
  for line in path.read_text(encoding="utf-8").split("\n"):
    if line.startswith(_PROMPT_SECTION_MARKER_PREFIX) and line.endswith(_PROMPT_SECTION_MARKER_SUFFIX):
      if current_id is not None:
        sections[current_id] = "\n".join(current_lines)
      current_id = line[len(_PROMPT_SECTION_MARKER_PREFIX):-len(_PROMPT_SECTION_MARKER_SUFFIX)]
      current_lines = []
      continue
    if current_id is not None:
      current_lines.append(line)
  if current_id is not None:
    sections[current_id] = "\n".join(current_lines)
  missing = [section_id for section_id in required if section_id not in sections]
  if missing:
    raise ValueError(
        f"{path} is missing required section(s): {', '.join(missing)} — the repo checkout "
        f"most likely predates the {extraction} extraction commit")
  return sections


def load_marker_sections(path: pathlib.Path, required: tuple[str, ...], *, extraction: str) -> dict[str, str]:
  """The public form of the marker-section loader (shared with the v2 assembly owner)."""
  return _load_prompt_sections(path, required, extraction=extraction)


def load_worker_prompt_sections(cfg: config.CharlieBotConfig) -> dict[str, str]:
  """Read the shared section sources fresh and split them into their required sections.

  ``prompts/task_base.md`` (the common execution rules' single maintained home) is read
  first, then ``prompts/worker.md``; a section id defined by both files is a template
  error, never a silent override.
  """
  worker_sections = _load_prompt_sections(
      cfg.charlie_bot_repo / "prompts" / "worker.md", (), extraction="worker-prompt")
  base_sections = _load_prompt_sections(
      cfg.charlie_bot_repo / "prompts" / "task_base.md", (), extraction="task-base-prompt")
  duplicate = sorted(set(base_sections) & set(worker_sections))
  if duplicate:
    raise ValueError("prompts/task_base.md and prompts/worker.md both define section(s): " + ", ".join(duplicate))
  merged = {**base_sections, **worker_sections}
  missing = [sid for sid in _REQUIRED_WORKER_PROMPT_SECTIONS if sid not in merged]
  if missing:
    raise ValueError("the worker prompt sections are missing required section(s): " + ", ".join(missing))
  return merged


def substitute_tokens(template: str, tokens: dict[str, str]) -> str:
  """Sequentially `str.replace` every `{{name}}` token (not `str.format` -- section text may
  contain literal single braces)."""

  result = template
  for token, value in tokens.items():
    result = result.replace(token, value)
  return result


def verify_contract_tokens(cfg: config.CharlieBotConfig) -> dict[str, str]:
  """verify.md's token map: the expected result trailer and the canonical plan template's path.

  One home for the verify rules segment: a token verify.md gains gets its value here once.
  """
  return {
      "{{result_trailer_expected}}": verify_trailer.VERIFY_RESULT_TRAILER_EXPECTED,
      "{{canonical_template_path}}": str((cfg.charlie_bot_repo / "prompts" / "plan_template.html").resolve()),
  }


def worktree_binding_tokens(
    *, intro_line: str, branch_name: str, base_branch_origin: str, wt_path: str, repo_path: str) -> dict[str, str]:
  """The bindings section's token map: the intro line plus branch/worktree/repo.

  One home for render_worktree_bindings, so a token the section gains gets its value once.
  """
  return {
      "{{intro_line}}": intro_line,
      "{{branch_name}}": branch_name,
      "{{base_branch_origin}}": base_branch_origin,
      "{{wt_path}}": wt_path,
      "{{repo_path}}": repo_path,
  }
