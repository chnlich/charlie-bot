"""The artifacts package's sidebar contribution: the pending-approval flag and the plan copy on fork."""

import json
import pathlib
import shutil

from src.features.artifacts import plan_paths, plans
from src.infra import json_utils, log_once
from src.runtime import sidebar_state
from src.runtime.hooks import sidebar_contributions

log = log_once.LazyStructlogLogger()


def has_pending_plan_approval_sync(plans_path: pathlib.Path, session_id: str) -> bool:
  """True if any lineage in the plans.json at *plans_path* is 'awaiting approval'.

  Delegates to the tolerant read in src.features.artifacts.plans (single authority for
  catch-and-derive). Any error entry is logged via ``plan_registry_read_failed``
  and contributes no pending approval. The probe must never raise — a corrupt
  single-session file cannot 5xx the sidebar poll for all sessions.
  """
  result = plans.read_plans_tolerant(plans_path, session_id)
  for error in result["errors"]:
    log.warning(
        "plan_registry_read_failed",
        session_id=error.get("session_id"),
        error=error.get("error"),
    )
  return any(plan.get("state") == plans.AWAITING_APPROVAL_STATE for plan in result["plans"])


def _copy_plans_to_child(parent_dir: pathlib.Path, child_session_dir: pathlib.Path) -> None:
  """Copy parent plans.json and every referenced artifact file into the child.

  The child registry rewrites each ``versions[].file`` to a POSIX path
  relative to the child session directory. All other registry fields carry
  over unchanged. A missing or outside-parent artifact logs a warning and
  does not abort the fork. No plans.json in the parent means nothing to copy.
  """
  parent_dir = parent_dir.resolve()
  parent_plans_path = parent_dir / "plans.json"
  if not parent_plans_path.exists():
    return
  _copy_plans_sync(parent_plans_path, parent_dir, child_session_dir.resolve())


def _copy_plans_sync(parent_plans_path: pathlib.Path, parent_dir: pathlib.Path, child_dir: pathlib.Path) -> None:
  raw = parent_plans_path.read_text(encoding="utf-8")
  data = json.loads(raw)
  # Every in-parent relative path must be reserved before any outside-parent
  # fallback is chosen, so a fallback can never alias an artifact a later
  # version copies.
  resolved: list[tuple[dict, dict, pathlib.Path, pathlib.Path | None]] = []
  reserved_relative_paths = {"plans.json"}
  for plan in data.get("plans", []):
    for ver in plan.get("versions", []):
      file_rel = ver.get("file")
      if not file_rel:
        continue
      candidate, normalized_rel = plan_paths.resolve_plan_file(parent_dir, file_rel)
      resolved.append((plan, ver, candidate, normalized_rel))
      if normalized_rel is not None:
        reserved_relative_paths.add(normalized_rel.as_posix())

  for plan, ver, candidate, resolved_rel in resolved:
    normalized_rel = resolved_rel
    inside_parent = normalized_rel is not None
    if normalized_rel is None:
      fallback_rel = plan_paths.fallback_relative_path(parent_dir, candidate)
      normalized_rel = fallback_rel
      suffix_number = 1
      while (normalized_rel.as_posix() in reserved_relative_paths or (child_dir / normalized_rel).exists()):
        normalized_rel = fallback_rel.with_name(f"{fallback_rel.name}.outside-{suffix_number}")
        suffix_number += 1
      reserved_relative_paths.add(normalized_rel.as_posix())
      log.warning(
          "plan_artifact_outside_parent_on_fork",
          file=str(candidate),
          relative_file=normalized_rel.as_posix(),
          plan=plan.get("id"),
          v=ver.get("v"),
      )
    src = parent_dir / normalized_rel
    dst = child_dir / normalized_rel
    ver["file"] = normalized_rel.as_posix()
    if not inside_parent:
      continue
    if not src.exists():
      log.warning("plan_artifact_missing_on_fork", file=str(src), plan=plan.get("id"), v=ver.get("v"))
      continue
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dst)
  child_plans_path = child_dir / "plans.json"
  child_plans_path.parent.mkdir(parents=True, exist_ok=True)
  json_utils.write_json_atomically(child_plans_path, data, indent=2)


class ArtifactsSidebar(sidebar_contributions.SidebarContribution):
  """The pending-plan-approval flag of a row, and the plan registry a fork carries over."""

  watched_files = ("plans.json",)

  def row_flags(self, session_dir: pathlib.Path, session_id: str) -> dict[str, bool]:
    pending = has_pending_plan_approval_sync(session_dir / "plans.json", session_id)
    return {sidebar_state.HAS_PENDING_PLAN_APPROVAL: pending}

  def copy_on_fork(self, parent_dir: pathlib.Path, child_dir: pathlib.Path) -> None:
    _copy_plans_to_child(parent_dir, child_dir)


contribution = ArtifactsSidebar()
