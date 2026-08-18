"""Worker/verify prompt assembly — template loading, marker-section extraction, token substitution."""

from pathlib import Path

from src.core.config import CharlieBotConfig
from src.core.memory import assemble_worker
from src.core.models import SessionMetadata, TaskType

_PROMPT_SECTION_MARKER_PREFIX = "<!-- section: "
_PROMPT_SECTION_MARKER_SUFFIX = " -->"

# Every workflow section draws from the same token map (intro + branch tokens);
# an absent key fails loud below rather than selecting a wrong workflow body.
# The id set lives only here: _REQUIRED_WORKER_PROMPT_SECTIONS derives it.
_WORKFLOW_PROMPT_SECTION = {
  TaskType.IMPLEMENT: "workflow_implement",
  TaskType.QUICK_EDIT: "workflow_quick_edit",
  TaskType.SCRIPT_RUN: "workflow_script_run",
}

_REQUIRED_WORKER_PROMPT_SECTIONS = (
  "session_info",
  "coding_principles",
  "skills_discovery",
  "remote_scratch",
  "role",
  "intro_new",
  "intro_continuation",
  "worktree_workflow_header",
  *_WORKFLOW_PROMPT_SECTION.values(),
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


def load_worker_prompt_sections(cfg: CharlieBotConfig) -> dict[str, str]:
  """Read prompts/worker.md fresh and split it into its required sections."""
  return _load_prompt_sections(
      cfg.charlie_bot_repo / "prompts" / "worker.md", _REQUIRED_WORKER_PROMPT_SECTIONS, extraction="worker-prompt")


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

  workflow_section_id = _WORKFLOW_PROMPT_SECTION.get(task_type)
  if workflow_section_id is None:
    raise ValueError(f"unsupported task_type: {task_type!r}")
  workflow_body = _substitute_tokens(sections[workflow_section_id], {"{{intro_line}}": intro_line, **branch_tokens})

  task_section = sections["task"].replace("{{description}}", description)

  worktree_section = (
      f"{sections['worktree_workflow_header']}\n{workflow_body}\n\n"
      f"{sections['task_spec_source_files']}\n{task_section}")

  iteration_reports_section = ""
  if loop_dir and iteration_number is not None:
    iteration_body = _substitute_tokens(
        sections["iteration_reports"], {
            "{{loop_dir}}": loop_dir,
            "{{iteration_number_padded}}": f"{iteration_number:04d}",
            "{{iteration_number}}": str(iteration_number),
        })
    iteration_reports_section = f"\n\n{iteration_body}"

  memory_section = ""
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
