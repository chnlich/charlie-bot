"""Worker/verify prompt assembly — template loading, marker-section extraction, token substitution.

This is the v1 (legacy session) worker assembly. It stays a compatibility caller during
the task-tree migration; the v2 task assembly lives in :mod:`src.core.task_prompts`. The
section sources are shared: the common execution rules moved to ``prompts/task_base.md``
(their single maintained home) and this loader merges that file's sections with
``prompts/worker.md``'s, so both assemblies read one set of canonical sections.
"""

from pathlib import Path

from src.core.config import CharlieBotConfig
from src.core.models import SessionMetadata, TaskType

_PROMPT_SECTION_MARKER_PREFIX = "<!-- section: "
_PROMPT_SECTION_MARKER_SUFFIX = " -->"

# Single home of the per-task-type worker.md section selection. Element 0 is
# the bindings section (the v1 assembly renders it with the branch tokens
# inline; the v2 assembly renders it per-run through render_worktree_bindings
# and joins elements 1: as the persistent workflow rule). A task type absent
# from the map fails loud rather than selecting a wrong workflow body.
# implement and quick_edit compose the template's shared workflow_steps body
# with their own closing STOP section, so the commit-message rule lives once
# in prompts/worker.md; script_run's body is one complete section.
# _REQUIRED_WORKER_PROMPT_SECTIONS derives its id set from this map.
WORKFLOW_PROMPT_SECTION = {
    TaskType.IMPLEMENT: ("worktree_bindings", "workflow_steps", "workflow_implement"),
    TaskType.QUICK_EDIT: ("worktree_bindings", "workflow_steps", "workflow_quick_edit"),
    TaskType.SCRIPT_RUN: ("workflow_script_run_bindings", "workflow_script_run"),
}

_REQUIRED_WORKER_PROMPT_SECTIONS = (
    "session_info",
    "coding_principles",
    "skills_discovery",
    "remote_scratch",
    "role",
    "intro_new",
    "intro_continuation",
    *dict.fromkeys(sid for ids in WORKFLOW_PROMPT_SECTION.values() for sid in ids),
    "task_spec_source_files",
    "task",
    "iteration_reports",
    "worktree_persistence",
    "memory",
)

_REQUIRED_VERIFY_PROMPT_SECTIONS = ("preamble", "scope")


def _load_prompt_sections(path: Path, required: tuple[str, ...], *, extraction: str) -> dict[str, str]:
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


def load_marker_sections(path: Path, required: tuple[str, ...], *, extraction: str) -> dict[str, str]:
  """The public form of the marker-section loader (shared with the v2 assembly owner)."""
  return _load_prompt_sections(path, required, extraction=extraction)


def load_worker_prompt_sections(cfg: CharlieBotConfig) -> dict[str, str]:
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


def _substitute_tokens(template: str, tokens: dict[str, str]) -> str:
  """Sequentially `str.replace` every `{{name}}` token (not `str.format` -- section text may
  contain literal single braces)."""
  result = template
  for token, value in tokens.items():
    result = result.replace(token, value)
  return result


def _require_tokens_resolved(assembled: str, *, prompt: str) -> None:
  """Guard the end of prompt assembly: a leftover `{{token}}` means the template's token set
  and the builder's token map disagree, and a half-built prompt must never reach a worker."""
  if "{{" in assembled:
    raise ValueError(prompt + " prompt assembly left an unresolved {{token}} in the output")


def verify_contract_tokens(cfg: CharlieBotConfig) -> dict[str, str]:
  """verify.md's token map: the expected result trailer and the canonical plan template's path.

  One home for both render paths (the v1 spawn assembly and the v2 verify rules segment):
  a token verify.md gains gets its value here once.
  """
  from src.core.verify_trailer import VERIFY_RESULT_TRAILER_EXPECTED
  return {
      "{{result_trailer_expected}}": VERIFY_RESULT_TRAILER_EXPECTED,
      "{{canonical_template_path}}": str((cfg.charlie_bot_repo / "prompts" / "plan_template.html").resolve()),
  }


def iteration_report_tokens(loop_dir: str, iteration_number: int) -> dict[str, str]:
  """The iteration_reports section's token map: loop dir plus the plain and zero-padded number.

  One home for both render paths (the v1 worker assembly and the v2 render_iteration_reports),
  so the padding contract is one definition.
  """
  return {
      "{{loop_dir}}": loop_dir,
      "{{iteration_number_padded}}": f"{iteration_number:04d}",
      "{{iteration_number}}": str(iteration_number),
  }


def _build_worker_prompt(
    description: str,
    repo_path: Path,
    base_branch: str,
    branch_name: str,
    wt_path: str,
    session_meta: SessionMetadata,
    cfg: CharlieBotConfig,
    task_type: TaskType,
    loop_dir: str | None,
    iteration_number: int | None,
    is_continuation: bool,
    keep_worktree: bool,
    start_point: str | None,
) -> str:
  """Build the task-specific worker prompt (session info + worktree workflow + task)."""
  sections = load_worker_prompt_sections(cfg)

  session_info = sections["session_info"].replace("{{session_name}}", session_meta.name)

  intro_line = sections["intro_continuation"] if is_continuation else sections["intro_new"]

  branch_origin = f"`{base_branch}`" + (f" @ `{start_point}`" if start_point else "")
  branch_tokens = {
      "{{branch_name}}": branch_name,
      "{{base_branch_origin}}": branch_origin,
      "{{wt_path}}": wt_path,
      "{{repo_path}}": str(repo_path),
  }

  workflow_section_ids = WORKFLOW_PROMPT_SECTION.get(task_type)
  if workflow_section_ids is None:
    raise ValueError(f"unsupported task_type: {task_type!r}")
  workflow_body = _substitute_tokens(
      "\n".join(sections[section_id] for section_id in workflow_section_ids), {
          "{{intro_line}}": intro_line,
          **branch_tokens
      })

  task_section = sections["task"].replace("{{description}}", description)

  worktree_section = (f"{workflow_body}\n\n{sections['task_spec_source_files']}\n{task_section}")

  iteration_reports_section = ""
  if loop_dir and iteration_number is not None:
    iteration_body = _substitute_tokens(
        sections["iteration_reports"], iteration_report_tokens(loop_dir, iteration_number))
    iteration_reports_section = f"\n\n{iteration_body}"

  memory_section = ""
  # lazy: keeps the memory store off the M99 server import floor (docs/perf_baseline.md)
  from src.core.memory import assemble_worker
  memory_block = assemble_worker(cfg.memory_dir, repo_path.name)
  if memory_block:
    memory_section = "\n" + sections["memory"].replace("{{memory_block}}", memory_block)

  keep_worktree_section = ""
  if keep_worktree:
    keep_worktree_section = f"\n\n{sections['worktree_persistence']}"

  result = (
      f"{session_info}\n{sections['coding_principles']}\n{sections['skills_discovery']}\n"
      f"{sections['remote_scratch']}\n{sections['role']}"
      f"{memory_section}\n{worktree_section}{iteration_reports_section}{keep_worktree_section}")

  _require_tokens_resolved(result, prompt="worker")

  return result
