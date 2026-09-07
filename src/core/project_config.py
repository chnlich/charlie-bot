"""Per-project instruction config: discovery, validation, and body loading.

An *enabled* project is a directory ``<charliebot_home>/projects/<group>/``
carrying a ``project.yaml``. The yaml names the project's bodies:

    prompt_file: project.md          # required, nonempty: the common rule body
    manager_prompt_file: manager.md  # optional, nonempty: manager-only supplement

Both values are relative paths (schema-enforced) resolved against the project
directory — the config file's own directory — and must stay confined to it,
symlinks included: an absolute value, a ``..`` climb, or a symlink out of the
project directory is an error. The two fields must not point at the same file
by any alias. Destination validity (relative, confined, not duplicated) applies
to every session; only a manager reads the supplement's content, so an
ordinary session neither reads nor requires its existence. Unknown keys, wrong
types, and an explicit ``manager_prompt_file: null`` are errors; only true
omission of the key means "no supplement".

Discovery is by file presence only: a missing ``project.yaml`` means the
project is not enabled and every session in that group keeps the pre-project
behavior. A present-but-broken state — a dangling project directory symlink
or config symlink, an unreadable or invalid config, or an unreadable
applicable body — raises :class:`ProjectInstructionError`; the caller turns
that into a clear per-turn failure; the next new turn re-reads the files.
"""

import hashlib
from dataclasses import dataclass
from pathlib import Path

import yaml
from pydantic import BaseModel, ConfigDict, field_validator, model_validator

from src.core.yaml_utils import load_yaml

PROJECTS_DIR_NAME = "projects"
PROJECT_CONFIG_FILENAME = "project.yaml"

# pathlib raises a plain RuntimeError (not OSError) when resolve() hits a
# symlink loop, and a ValueError for an embedded NUL; all of these mean "this
# path cannot be resolved" and all of them are config errors, never raw
# escapes past ProjectInstructionError.
_RESOLVE_ERRORS = (OSError, RuntimeError, ValueError)


class ProjectInstructionError(Exception):
  """An enabled project's config or an applicable body cannot be loaded.

  The message names the file and the reason; it is a per-turn failure, never a
  silent skip or a fallback to stale content.
  """


class ProjectConfig(BaseModel):
  """Body pointers of one enabled project; the yaml carries nothing else.

  Both values must be relative paths, resolved against the project directory.
  ``manager_prompt_file`` may be omitted; an explicit ``null`` is invalid, so
  an omitted key and a present-but-null key are distinguishable.
  """

  model_config = ConfigDict(extra="forbid")

  prompt_file: str
  manager_prompt_file: str | None = None

  @field_validator("prompt_file", "manager_prompt_file")
  @classmethod
  def _nonempty(cls, v: str | None) -> str | None:
    if v is not None and not v.strip():
      raise ValueError("must be a nonempty string")
    return v

  @field_validator("prompt_file", "manager_prompt_file")
  @classmethod
  def _relative(cls, v: str | None) -> str | None:
    if v is not None and Path(v).is_absolute():
      raise ValueError(f"must be a relative path resolved against the project directory, got {v!r}")
    return v

  @model_validator(mode="after")
  def _no_explicit_null_supplement(self) -> "ProjectConfig":
    if "manager_prompt_file" in self.model_fields_set and self.manager_prompt_file is None:
      raise ValueError("manager_prompt_file must be omitted or a nonempty string, not null")
    return self


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


def _resolve_confined(project_dir: Path, candidate: Path, what: str) -> Path:
  """Resolve *candidate* and require its real path to stay inside *project_dir*.

  Resolution follows symlinks to their real target and must stay inside
  *project_dir*: a ``..`` climb or a symlink out of the project directory
  fails here. Where an unusable candidate fails is interpreter-dependent, and
  both failure points raise ProjectInstructionError. Embedded NUL raises
  ValueError inside resolve() on every Python. A symlink loop raises
  RuntimeError inside resolve() on Python 3.12, while on 3.13 resolve() folds
  the loop into itself and it surfaces as OSError at the body read. A missing
  intermediate passes resolve() on both and surfaces at the body read. The
  project directory itself may be a symlink: confinement is checked against
  its resolved target, so a body can still only come from inside the
  (possibly redirected) project directory.
  """
  try:
    resolved = candidate.resolve()
    base = project_dir.resolve()
  except _RESOLVE_ERRORS as e:
    raise ProjectInstructionError(f"project {what} cannot be resolved: {candidate} ({e})") from e
  if not resolved.is_relative_to(base):
    raise ProjectInstructionError(f"project {what} resolves to {resolved}, outside the project directory {project_dir}")
  return resolved


def _body_destination(project_dir: Path, value: str, field_name: str) -> Path:
  """Resolve one configured body destination inside *project_dir* without reading it.

  The declared value is relative (schema-enforced) and resolves against the
  project directory, symlinks included. Existence and readability are
  deliberately NOT checked here — the caller decides whether the body's
  content is applicable (an ordinary session never reads the manager-only
  supplement and must not require its existence).
  """
  return _resolve_confined(project_dir, project_dir / value, field_name)


def _read_body(destination: Path, field_name: str) -> ProjectBody:
  """Read one resolved body's full text; failures name the file and reason."""
  try:
    text = destination.read_text(encoding="utf-8")
  except (OSError, UnicodeDecodeError) as e:
    raise ProjectInstructionError(f"project {field_name} body unreadable: {destination} ({e})") from e
  return ProjectBody(path=destination, text=text)


def load_project_bodies(home: Path, group: str, *, manager: bool) -> ProjectBodies | None:
  """Load the instruction bodies one grouped session's turn needs, or ``None``.

  ``None`` means the project is not enabled (no ``project.yaml``): the caller
  keeps the pre-project behavior. A dangling ``project.yaml`` symlink is a
  present-but-broken config, and so is a dangling ``projects/<group>``
  directory symlink — both fail loudly instead of silently disabling the
  project. Every session validates both configured destinations (relative,
  confined, not the same file by any alias); only a manager reads the
  supplement, so an ordinary session neither reads nor requires its existence.
  Every failure raises :class:`ProjectInstructionError` naming the file and
  reason.
  """
  dir_ = project_dir(home, group)
  config_path = dir_ / PROJECT_CONFIG_FILENAME
  if not config_path.exists() and not config_path.is_symlink():
    # Absent project.yaml. A dangling (or otherwise unresolvable) project
    # directory symlink reads identically here — stat on the config path
    # fails either way — but it is a present-but-broken directory, never "not
    # enabled". This is the directory-level twin of the dangling config
    # symlink check below.
    if dir_.is_symlink() and not dir_.exists():
      raise ProjectInstructionError(f"project directory is a broken symlink: {dir_} -> {dir_.readlink()}")
    return None
  # Present from here on. A dangling config symlink must not read as "not
  # enabled": discovery by file presence counts a present-but-broken file as
  # an error, never as a silent fallback to the pre-project behavior.
  if config_path.is_symlink() and not config_path.exists():
    raise ProjectInstructionError(f"project config is a broken symlink: {config_path} -> {config_path.readlink()}")
  _resolve_confined(dir_, config_path, "config")
  try:
    data = load_yaml(config_path)
  except (OSError, UnicodeDecodeError, yaml.YAMLError) as e:
    raise ProjectInstructionError(f"project config unreadable: {config_path} ({e})") from e
  if not isinstance(data, dict):
    raise ProjectInstructionError(f"project config must be a mapping: {config_path}")
  try:
    cfg = ProjectConfig(**data)
  except Exception as e:
    raise ProjectInstructionError(f"invalid project config: {config_path} ({e})") from e
  common_path = _body_destination(dir_, cfg.prompt_file, "prompt_file")
  supplement_path: Path | None = None
  if cfg.manager_prompt_file is not None:
    supplement_path = _body_destination(dir_, cfg.manager_prompt_file, "manager_prompt_file")
    if supplement_path == common_path:
      raise ProjectInstructionError(
          f"project config points prompt_file and manager_prompt_file at the same file: {common_path}")
  common = _read_body(common_path, "prompt_file")
  supplement: ProjectBody | None = None
  if manager and supplement_path is not None:
    supplement = _read_body(supplement_path, "manager_prompt_file")
  return ProjectBodies(config_path=config_path, common=common, manager_supplement=supplement)


def content_sha256(text: str) -> str:
  """The content hash recorded next to a body's source path in diagnostics."""
  return hashlib.sha256(text.encode("utf-8")).hexdigest()
