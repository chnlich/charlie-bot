"""Package structure check: the groups of ``src`` and the direction each group may import.

Every ``.py`` file under ``src``, and ``server.py``, belongs to one group:

    infra         src/infra, plus src/__init__.py and the markers of src/backends and src/features
    runtime       src/runtime and its subpackages
    backends.<x>  src/backends/<x>
    features.<x>  src/features/<x> and its subpackages
    app           src/app and server.py

Directions: infra imports infra only. runtime imports runtime and infra. A backend or a feature imports itself,
runtime and infra. The backends in ``BACKEND_VARIANTS`` may also import backends.claude_code, and the features in
``CHAT_CHANNELS`` may also import features.chat_threads. app imports every group.

An import counts where it stands: module level, function body, ``if TYPE_CHECKING`` block, relative form. A string
whose whole value is a dotted ``src.`` module path counts as an import of the longest existing module: lazy
loaders, the CLI command table and patch targets name modules this way. A longer string counts one import per
``src.`` module path embedded in it, the pieces of an f-string included. A docstring — the first statement of a
module, class or function, when that statement is a bare string — counts no reference. A module path built at
runtime hides its target from this check, so building one fails it.

structure_exceptions.txt records the reverse imports that predate the groups: one row per
``importer file -> imported file`` pair.
"""

import ast
import re
import textwrap
from pathlib import Path
from typing import NamedTuple

import conftest
import pytest

BACKEND_VARIANTS = frozenset({"kimi", "openai_compatible"})
CHAT_CHANNELS = frozenset({"slack", "discord"})
CONTAINER_MARKERS = frozenset({"src/__init__.py", "src/backends/__init__.py", "src/features/__init__.py"})
EXCEPTIONS_PATH = Path(__file__).with_name("structure_exceptions.txt")
MODULE_PATH = re.compile(r"src(\.[A-Za-z_]\w*)+")
EMBEDDED = re.compile(r"(?<![\w.])src(?:\.[A-Za-z_]\w*)+")
DEPEND_ON_THE_RUNTIME = (
    "Depend on the runtime's public interface: the imported package registers its "
    "implementation with the runtime, and the importer looks it up there.")


class Reference(NamedTuple):
  """One import of a module under src: the importing file and line, and the file it names (repo-relative)."""
  importer: str
  line: int
  target: str


class BuiltPath(NamedTuple):
  file: str
  line: int


class Structure(NamedTuple):
  ungrouped: list[str]
  references: list[Reference]
  built_paths: list[BuiltPath]


def group_of(rel: str) -> str | None:
  """The group of a repo-relative ``.py`` path: infra, runtime, app, ``backends.<x>`` or ``features.<x>``."""
  parts = rel.split("/")
  if rel == "server.py":
    return "app"
  if rel in CONTAINER_MARKERS:
    return "infra"
  if parts[0] != "src" or len(parts) < 3:
    return None
  if parts[1] in ("infra", "runtime", "app"):
    return parts[1]
  if parts[1] in ("backends", "features") and len(parts) >= 4:
    return f"{parts[1]}.{parts[2]}"
  return None


def allowed_groups(group: str) -> set[str] | None:
  """The groups ``group`` may import besides itself; None for app, which may import every group."""
  kind, _, name = group.partition(".")
  if kind == "app":
    return None
  if kind == "infra":
    return set()
  if kind == "runtime":
    return {"infra"}
  if kind == "backends":
    return {"runtime", "infra"} | ({"backends.claude_code"} if name in BACKEND_VARIANTS else set())
  if kind == "features":
    return {"runtime", "infra"} | ({"features.chat_threads"} if name in CHAT_CHANNELS else set())
  raise ValueError(f"unknown group {group}")


def module_index(root: Path) -> dict[str, str]:
  """Dotted module name -> repo-relative path, for every ``.py`` file under src and for server.py."""
  index = {"server": "server.py"}
  for path in sorted((root / "src").rglob("*.py")):
    rel = path.relative_to(root)
    index[".".join(rel.with_suffix("").parts).removesuffix(".__init__")] = rel.as_posix()
  return index


def resolve(name: str, index: dict[str, str]) -> str | None:
  """The repo-relative path of the longest existing module that ``name`` starts with, or None."""
  while name:
    if name in index:
      return index[name]
    name = name.rpartition(".")[0]
  return None


def absolute_module(node: ast.ImportFrom, package: str) -> str:
  """The dotted module a ``from`` statement names, with a relative form resolved against ``package``."""
  if node.level == 0:
    return node.module
  base = package.split(".")[:len(package.split(".")) - node.level + 1]
  return ".".join([*base, *([node.module] if node.module else [])])


def named_modules(node: ast.AST, package: str, index: dict[str, str]) -> list[str]:
  """The dotted names one node imports: an import statement, or the ``src.`` module paths a string names."""
  if isinstance(node, ast.Import):
    return [alias.name for alias in node.names]
  if isinstance(node, ast.ImportFrom):
    module = absolute_module(node, package)
    return [f"{module}.{alias.name}" if f"{module}.{alias.name}" in index else module for alias in node.names]
  if isinstance(node, ast.Constant) and isinstance(node.value, str):
    if MODULE_PATH.fullmatch(node.value):
      return [node.value]
    return [match.group(0) for match in EMBEDDED.finditer(node.value)]
  return []


def is_module_prefix(node: ast.AST) -> bool:
  """A string literal that opens a module path and stops at a dot: the rest of the path comes from a variable."""
  return (
      isinstance(node, ast.Constant) and isinstance(node.value, str) and node.value.startswith("src.") and
      node.value.endswith("."))


def builds_module_path(node: ast.AST) -> bool:
  """An f-string or a concatenation that completes a ``src.`` prefix with a variable."""
  if isinstance(node, ast.JoinedStr):
    return any(
        is_module_prefix(part) and isinstance(following, ast.FormattedValue)
        for part, following in zip(node.values, node.values[1:], strict=False))
  return isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add) and is_module_prefix(node.left)


def docstring_nodes(tree: ast.Module) -> set[int]:
  """The ids of the docstring constants: the first statement of a module, class or function, a bare string."""
  ids = set()
  for node in ast.walk(tree):
    if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) and node.body:
      first = node.body[0]
      if (isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant) and isinstance(first.value.value, str)):
        ids.add(id(first.value))
  return ids


def scan(root: Path) -> Structure:
  index = module_index(root)
  structure = Structure([], [], [])
  for rel in index.values():
    if group_of(rel) is None:
      structure.ungrouped.append(rel)
    package = ".".join(Path(rel).with_suffix("").parts[:-1])
    found = set()
    tree = ast.parse((root / rel).read_text(encoding="utf-8"), filename=rel)
    docstrings = docstring_nodes(tree)
    for node in ast.walk(tree):
      if id(node) in docstrings:
        continue
      for name in named_modules(node, package, index):
        target = resolve(name, index)
        if target is not None and target != rel:
          found.add(Reference(rel, node.lineno, target))
      if builds_module_path(node):
        structure.built_paths.append(BuiltPath(rel, node.lineno))
    structure.references.extend(sorted(found))
  return structure


def violations(structure: Structure) -> list[Reference]:
  """The references that cross groups against the directions."""
  found = []
  for reference in structure.references:
    importer, imported = group_of(reference.importer), group_of(reference.target)
    if importer is None or imported is None or importer == imported:
      continue
    allowed = allowed_groups(importer)
    if allowed is not None and imported not in allowed:
      found.append(reference)
  return found


def recorded_pairs() -> set[tuple[str, str]]:
  pairs = set()
  for line in EXCEPTIONS_PATH.read_text(encoding="utf-8").splitlines():
    if line.strip() and not line.startswith("#"):
      importer, _, imported = line.partition(" -> ")
      pairs.add((importer.strip(), imported.strip()))
  return pairs


# The synthetic tree of the scan test: runtime/core.py names one module of features/alpha once per line below, in
# every import form, as a whole string, and embedded in a command string and a prose f-string, then builds two
# module paths (the "src.features." prefixes still name the features marker). The docstring of `documented` names
# the module too, and counts nothing.
EMPTY_MODULES = (
    "server.py", "src/__init__.py", "src/runtime/__init__.py", "src/features/__init__.py",
    "src/features/alpha/__init__.py", "src/features/alpha/impl.py")
FORMS_SOURCE = textwrap.dedent(
    """\
    from typing import TYPE_CHECKING

    from src.features.alpha import impl
    if TYPE_CHECKING:
      from src.features.alpha.impl import Thing


    def local(name):
      import src.features.alpha.impl as local_impl
      from ..features.alpha import impl as relative_impl
      lazy = "src.features.alpha.impl"
      command = "uv run --no-sync python -m src.features.alpha.impl run"
      prose = f"src.features.alpha.impl {name} drifted"
      joined = "src.features." + name
      return f"src.features.{name}"


    def documented():
      \"""Delegates to src.features.alpha.impl for the real work.\"""
      return 1
    """)


@pytest.fixture(scope="module")
def structure() -> Structure:
  return scan(conftest.ROOT)


def test_every_python_file_is_in_a_group(structure: Structure) -> None:
  assert not structure.ungrouped, "\n".join(
      f"{rel}: sits outside the groups src/infra, src/runtime, src/backends/<name>, src/features/<name>, "
      "src/app; move it into one." for rel in structure.ungrouped)


def test_imports_follow_the_directions(structure: Structure) -> None:
  recorded = recorded_pairs()
  failures = []
  for reference in violations(structure):
    if (reference.importer, reference.target) in recorded:
      continue
    importer, imported = group_of(reference.importer), group_of(reference.target)
    may = sorted({importer, *allowed_groups(importer)})
    failures.append(
        f"{reference.importer}:{reference.line}: group {importer} imports {reference.target} "
        f"(group {imported}), and {importer} may import only {', '.join(may)}. {DEPEND_ON_THE_RUNTIME}")
  assert not failures, "\n".join(failures)


def test_every_recorded_reverse_import_still_exists(structure: Structure) -> None:
  current = {(reference.importer, reference.target) for reference in violations(structure)}
  failures = [
      f"{importer}: the reverse import of {imported} is gone, or no longer breaks a direction; remove its row."
      for importer, imported in sorted(recorded_pairs() - current)
  ]
  assert not failures, "\n".join(failures)


def test_no_module_path_is_built_at_runtime(structure: Structure) -> None:
  assert not structure.built_paths, "\n".join(
      f"{built.file}:{built.line}: builds a module path at runtime; this check cannot see its target. "
      "Import the module by its full name instead." for built in structure.built_paths)


def test_the_scan_counts_every_form_of_import(tmp_path: Path) -> None:
  """An import form the scan skips would let a reverse import in unseen."""
  modules = dict.fromkeys(EMPTY_MODULES, "")
  modules["src/runtime/core.py"] = FORMS_SOURCE
  for rel, source in modules.items():
    (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
    (tmp_path / rel).write_text(source, encoding="utf-8")

  found = scan(tmp_path)

  assert found.ungrouped == []
  assert {(r.importer, r.line, r.target) for r in found.references} == (
      {("src/runtime/core.py", line, "src/features/alpha/impl.py") for line in (3, 5, 9, 10, 11, 12, 13)} |
      {("src/runtime/core.py", line, "src/features/__init__.py") for line in (14, 15)})
  assert [built.line for built in found.built_paths] == [14, 15]
  assert {(r.importer, r.target) for r in violations(found)} == {("src/runtime/core.py", "src/features/alpha/impl.py")}
