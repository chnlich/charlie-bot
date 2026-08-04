"""The text surfaces the model reads offer one form for a file link, and one way to source its path.

The assertions are about the surface as a whole rather than about one skill's wording: a second
documented prefix, or a second existence-check instruction, is a choice the writer should not have.
"""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SURFACE_DIRS = ("skills", "prompts")
SURFACE_GLOBS = ("*.md", "*.html")
FILE_SERVER_SKILL = ROOT / "skills" / "file-server" / "SKILL.md"

# The rule that a link's path comes from this turn's command output: an ls, scoped to the exact
# path, taken right before it is pasted.
SOURCING_RE = re.compile(r"`ls`[^.]*exact[^.]*about to be pasted")


def _surface_files() -> list[Path]:
  files: list[Path] = []
  for name in SURFACE_DIRS:
    for pattern in SURFACE_GLOBS:
      files.extend(sorted((ROOT / name).rglob(pattern)))
  assert files, "no writing surface found to check"
  return files


def _flattened(path: Path) -> str:
  """The file as one line, so a rule wrapped across lines still reads as one sentence."""
  return " ".join(path.read_text(encoding="utf-8").split())


def test_the_written_file_link_form_is_the_absolute_filepath_prefix() -> None:
  documenting = [path for path in _surface_files() if "/absolute_filepath/" in path.read_text(encoding="utf-8")]
  assert FILE_SERVER_SKILL in documenting
  assert "<base_url>/absolute_filepath/" in FILE_SERVER_SKILL.read_text(encoding="utf-8")


def test_the_legacy_prefix_appears_only_where_the_alias_is_documented() -> None:
  legacy: list[tuple[Path, int, str]] = []
  for path in _surface_files():
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
      if "/files/" in line:
        legacy.append((path, number, line))
  assert len(legacy) == 1, f"the legacy prefix is documented more than once: {legacy}"
  path, _, line = legacy[0]
  assert path == FILE_SERVER_SKILL
  assert "alias" in line.lower(), line


def test_the_sourcing_rule_requires_ls_on_the_exact_path() -> None:
  carrying = [path for path in _surface_files() if SOURCING_RE.search(_flattened(path))]
  assert carrying == [FILE_SERVER_SKILL], f"the sourcing rule is missing or restated: {carrying}"


def test_every_existence_check_in_the_file_link_surface_names_the_exact_path() -> None:
  sentences = [part for part in re.split(r"(?<=[.:])\s", _flattened(FILE_SERVER_SKILL)) if "`ls`" in part]
  assert sentences, "the file-server skill says nothing about checking the path"
  for sentence in sentences:
    assert "exact" in sentence, f"a looser existence check survives: {sentence}"
