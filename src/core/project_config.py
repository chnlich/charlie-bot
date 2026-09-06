"""Per-project instruction config: discovery, validation, and body loading.

An *enabled* project is a directory ``<charliebot_home>/projects/<group>/``
carrying a ``project.yaml``. The yaml names the project's bodies:

    prompt_file: project.md          # required, nonempty: the common rule body
    manager_prompt_file: manager.md  # optional, nonempty: manager-only supplement

Body paths resolve relative to the project directory (the config file's own
directory) and must stay confined to it, symlinks included; the two fields must
not point at the same file; unknown keys and wrong types are errors.

Discovery is by file presence only: a missing ``project.yaml`` means the
project is not enabled and every session in that group keeps the pre-project
behavior. A present but unreadable or invalid config, or an unreadable
applicable body, raises :class:`ProjectInstructionError` — the caller turns
that into a clear per-turn failure; the next new turn re-reads the files.
"""

import hashlib
from dataclasses import dataclass
from pathlib import Path

import yaml
from pydantic import BaseModel, ConfigDict, field_validator

from src.core.yaml_utils import load_yaml

PROJECTS_DIR_NAME = "projects"
PROJECT_CONFIG_FILENAME = "project.yaml"


class ProjectInstructionError(Exception):
  """An enabled project's config or an applicable body cannot be loaded.

  The message names the file and the reason; it is a per-turn failure, never a
  silent skip or a fallback to stale content.
  """


class ProjectConfig(BaseModel):
  """Body pointers of one enabled project; the yaml carries nothing else."""

  model_config = ConfigDict(extra="forbid")

  prompt_file: str
  manager_prompt_file: str | None = None

  @field_validator("prompt_file", "manager_prompt_file")
  @classmethod
  def _nonempty(cls, v: str | None) -> str | None:
    if v is not None and not v.strip():
      raise ValueError("must be a nonempty string")
    return v


@dataclass(frozen=True)
class ProjectBody:
  """One instruction body: its verified source path and full text."""

  path: Path
  text: str


@dataclass(frozen=True)
class ProjectBodies:
  """The bodies one session's turn injects, resolved at call time."""

  config_path: Path
  common: ProjectBody
  manager_supplement: ProjectBody | None


def validate_group_name(group: str) -> None:
  """Reject a *group* that cannot serve as one directory name under projects/.

  ``/`` and ``\\`` would traverse, ``.`` and ``..`` escape, and an embedded NUL
  is never a usable name; anything else is a legal single directory segment.
  """
  if not group or group in (".", "..") or "/" in group or "\\" in group or "\x00" in group:
    raise ProjectInstructionError(f"group name {group!r} is not a safe project directory name")


def project_dir(home: Path, group: str) -> Path:
  """The project directory for *group* under *home*, after the name check."""
  validate_group_name(group)
  return home / PROJECTS_DIR_NAME / group


def _read_confined_body(project_dir: Path, value: str, field_name: str) -> ProjectBody:
  """Resolve one body pointer inside *project_dir* and read its full text.

  The declared value is relative to the project directory. Resolution follows
  symlinks to their real target, which must stay inside the real project
  directory — an absolute value, a ``..`` climb, or a symlink out of the
  project directory all fail here. Reading failures name the file and reason.
  """
  candidate = project_dir / value
  resolved = candidate.resolve()
  if not resolved.is_relative_to(project_dir.resolve()):
    raise ProjectInstructionError(
        f"project {field_name} {value!r} resolves to {resolved}, outside the project directory {project_dir}")
  try:
    text = resolved.read_text(encoding="utf-8")
  except (OSError, UnicodeDecodeError) as e:
    raise ProjectInstructionError(f"project {field_name} body unreadable: {resolved} ({e})") from e
  return ProjectBody(path=resolved, text=text)


def load_project_bodies(home: Path, group: str, *, manager: bool) -> ProjectBodies | None:
  """Load the instruction bodies one grouped session's turn needs, or ``None``.

  ``None`` means the project is not enabled (no ``project.yaml``): the caller
  keeps the pre-project behavior. With ``manager`` the configured supplement is
  loaded too; an unconfigured supplement loads nothing and matters only for a
  manager. Every failure raises :class:`ProjectInstructionError` naming the
  file and reason.
  """
  dir_ = project_dir(home, group)
  config_path = dir_ / PROJECT_CONFIG_FILENAME
  if not config_path.exists():
    return None
  try:
    data = load_yaml(config_path)
  except (OSError, yaml.YAMLError) as e:
    raise ProjectInstructionError(f"project config unreadable: {config_path} ({e})") from e
  if not isinstance(data, dict):
    raise ProjectInstructionError(f"project config must be a mapping: {config_path}")
  try:
    cfg = ProjectConfig(**data)
  except Exception as e:
    raise ProjectInstructionError(f"invalid project config: {config_path} ({e})") from e
  common = _read_confined_body(dir_, cfg.prompt_file, "prompt_file")
  supplement: ProjectBody | None = None
  if manager and cfg.manager_prompt_file is not None:
    supplement = _read_confined_body(dir_, cfg.manager_prompt_file, "manager_prompt_file")
    if supplement.path == common.path:
      raise ProjectInstructionError(
          f"project config points prompt_file and manager_prompt_file at the same file: {common.path}")
  return ProjectBodies(config_path=config_path, common=common, manager_supplement=supplement)


def content_sha256(text: str) -> str:
  """The content hash recorded next to a body's source path in diagnostics."""
  return hashlib.sha256(text.encode("utf-8")).hexdigest()
