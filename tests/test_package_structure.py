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

Inside runtime, ``RUNTIME_LAYERS`` puts every module in one of 8 layers, and a module imports only modules of its own
layer or a lower layer. The import graph of src/runtime has no cycle, and neither has the import graph of src/infra.
A runtime module that no entry of the table covers fails a test, and so does an entry that covers no module. A
package ``__init__.py`` is in no layer and is exempt as importer and as target of both rules.

An import statement inside a function body in src/runtime or src/infra that names a src module belongs at the module
top. It stays in the function only when a measurement shows that the module-top form slows the startup of one command.
Then it carries the comment ``# deferred: <command>``, for example ``# deferred: charliebot improve --help``. The
comment stands at the end of the first line of the statement or alone on the line directly above it. It names the
command and holds no number: the measured cost goes into the commit message of the change that adds the deferral. A
function-level import of a standard-library or third-party module needs no comment.

An import counts where it stands: module level, function body, ``if TYPE_CHECKING`` block, relative form. A string
whose whole value is a dotted ``src.`` module path counts as an import of the longest existing module: lazy
loaders, the CLI command table and patch targets name modules this way. A longer string counts one import per
``src.`` module path embedded in it, the pieces of an f-string included. A docstring — the first statement of a
module, class or function, when that statement is a bare string — counts no reference. A module path built at
runtime hides its target from this check, so building one fails it. The layer rule and the cycle rule count imports
the same way.

structure_exceptions.txt records the reverse imports that predate the groups: one row per
``importer file -> imported file`` pair. It holds group directions only: the layer and cycle checks have no exception
table, and a break is fixed in the code.
"""

import ast
import io
import re
import textwrap
import tokenize
from pathlib import Path
from typing import NamedTuple

import conftest
import pytest

BACKEND_VARIANTS = frozenset({"kimi", "openai_compatible"})
CHAT_CHANNELS = frozenset({"slack", "discord"})
CONTAINER_MARKERS = frozenset({"src/__init__.py", "src/backends/__init__.py", "src/features/__init__.py"})
EXCEPTIONS_PATH = Path(__file__).with_name("structure_exceptions.txt")
DEFERRAL_COMMENT = re.compile(r"#\s*deferred:\s*\S")
DEFERRAL_SCOPES = ("src/runtime/", "src/infra/")
MODULE_PATH = re.compile(r"src(\.[A-Za-z_]\w*)+")
EMBEDDED = re.compile(r"(?<![\w.])src(?:\.[A-Za-z_]\w*)+")
DEPEND_ON_THE_RUNTIME = (
    "Depend on the runtime's public interface: the imported package registers its "
    "implementation with the runtime, and the importer looks it up there.")
FIX_THE_UPWARD_IMPORT = (
    "Fix it in one of four ways. Give the lower module a narrow protocol that the higher module satisfies. "
    "Give the owning module an instance accessor. Move the shared code to the lower module. Delete the forwarder.")


class Layer(NamedTuple):
  """One layer of the runtime: its number, its name, and its entries. An entry is a module name relative to
  ``src.runtime``, or a package name that covers every module below it."""
  number: int
  name: str
  entries: tuple[str, ...]


# The highest layer comes first. A module imports modules of its own layer or a lower layer.
RUNTIME_LAYERS = (
    Layer(8, "entry points", ("api", "cli", "init")),
    Layer(
        7, "execution", (
            "autonamer", "master_cc", "master_cc_queue", "master_cc_run", "master_trigger", "spawner", "task_execution",
            "task_recovery", "triggers")),
    Layer(
        6, "task tree", ("session_dispatch", "task_completion", "task_prompts", "task_sessions", "worker_transcript")),
    Layer(
        5, "session services", (
            "control_sink", "scheduled_sessions", "session_anchors", "session_fork", "session_lifecycle",
            "session_listing", "session_search", "session_sidebar", "session_successor", "spawner_backends",
            "takeoff_gate")),
    Layer(
        4, "agent processes", (
            "agent_environment", "agent_process.base", "agent_process.deferred_build", "agent_process.spawn",
            "master_cc_state", "session_events", "streaming", "worker")),
    Layer(
        3, "records and events", (
            "control_events", "home_writer_fence", "launch_loop", "message_aggregator", "message_projection", "review",
            "runs", "session_store", "session_usage", "spawner_prompt", "trigger_files")),
    Layer(2, "hooks", ("hooks", "templating")),
    Layer(
        1, "foundation", (
            "agent_process.pty_common", "chat_events", "file_urls", "init_seed", "message_events",
            "model_family", "run_identity", "run_token", "sidebar_state", "task_errors", "thinking_state",
            "v1_sessions", "verify_trailer", "worktree_trash")),
)


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


def runtime_name(rel: str) -> str | None:
  """The name of a runtime module relative to ``src.runtime``; None for another file and for a package ``__init__``."""
  if not rel.startswith("src/runtime/") or rel.endswith("/__init__.py"):
    return None
  return ".".join(Path(rel).with_suffix("").parts[2:])


def runtime_files(root: Path) -> list[str]:
  """The repo-relative paths of the runtime modules under ``root``."""
  return [rel for rel in module_index(root).values() if runtime_name(rel) is not None]


def covers(entry: str, module: str) -> bool:
  """Whether a layer entry names ``module`` or the package that holds it."""
  return module == entry or module.startswith(f"{entry}.")


def layer_of(module: str, layers: tuple[Layer, ...]) -> Layer | None:
  """The one layer whose entries cover the runtime module ``module``; None when no layer covers it."""
  holding = [layer for layer in layers if any(covers(entry, module) for entry in layer.entries)]
  if len(holding) > 1:
    raise ValueError(f"{module} is covered by layers {[layer.number for layer in holding]}; keep it in one.")
  return holding[0] if holding else None


def layer_breaks(structure: Structure, layers: tuple[Layer, ...]) -> list[str]:
  """One failure line per reference from a runtime module to a module of a higher layer. A module that no layer
  covers is left to the completeness check."""
  failures = []
  for reference in structure.references:
    importer, imported = runtime_name(reference.importer), runtime_name(reference.target)
    if importer is None or imported is None:
      continue
    low, high = layer_of(importer, layers), layer_of(imported, layers)
    if low is None or high is None or high.number <= low.number:
      continue
    failures.append(
        f"{reference.importer}:{reference.line}: layer {low.number} ({low.name}) module {reference.importer} "
        f"imports {reference.target} (layer {high.number}, {high.name}); layer {low.number} may import only "
        f"layers 1 to {low.number}. {FIX_THE_UPWARD_IMPORT}")
  return failures


def unplaced_modules(files: list[str], layers: tuple[Layer, ...]) -> list[str]:
  """One failure line per runtime file that no layer covers."""
  return [
      f"{rel}: no layer in the runtime layer table; add it to the layer of the modules it serves." for rel in files
      if layer_of(runtime_name(rel), layers) is None
  ]


def stale_entries(files: list[str], layers: tuple[Layer, ...]) -> list[str]:
  """One failure line per layer entry that covers no runtime file."""
  modules = [runtime_name(rel) for rel in files]
  return [
      f"{entry}: names no runtime module; remove it from the layer table." for layer in layers
      for entry in layer.entries if not any(covers(entry, module) for module in modules)
  ]


def strongly_connected(edges: dict[str, dict[str, int]]) -> list[list[str]]:
  """The groups of two or more modules that all reach each other along ``edges`` (importer -> target -> line)."""
  order: dict[str, int] = {}
  low: dict[str, int] = {}
  stack: list[str] = []
  groups = []

  def visit(node: str) -> None:
    order[node] = low[node] = len(order)
    stack.append(node)
    for target in edges.get(node, {}):
      if target not in order:
        visit(target)
        low[node] = min(low[node], low[target])
      elif target in stack:
        low[node] = min(low[node], order[target])
    if low[node] == order[node]:
      group = [stack.pop()]
      while group[-1] != node:
        group.append(stack.pop())
      if len(group) > 1:
        groups.append(group)

  for node in sorted(edges):
    if node not in order:
      visit(node)
  return groups


def import_cycles(structure: Structure, package: str) -> list[str]:
  """One failure message per group of modules under the path prefix ``package`` that import each other in a ring.
  It names the members, then gives one ``importer:line -> target`` line for each edge inside the group."""
  edges: dict[str, dict[str, int]] = {}
  for reference in structure.references:
    if all(path.startswith(package) and not path.endswith("/__init__.py")
           for path in (reference.importer, reference.target)):
      edges.setdefault(reference.importer, {}).setdefault(reference.target, reference.line)
  failures = []
  for group in strongly_connected(edges):
    members = sorted(group)
    lines = [
        f"  {importer}:{line} -> {target}" for importer in members for target, line in edges[importer].items()
        if target in group
    ]
    failures.append("\n".join([f"import cycle among {len(members)} modules: {', '.join(members)}", *lines]))
  return failures


def function_level_imports(tree: ast.Module) -> list[ast.Import | ast.ImportFrom]:
  """The import statements inside a function body, nested functions included, in source order."""
  found: list[ast.Import | ast.ImportFrom] = []

  def visit(node: ast.AST, *, in_function: bool) -> None:
    for child in ast.iter_child_nodes(node):
      if not isinstance(child, (ast.stmt, ast.excepthandler, ast.match_case)):
        continue  # an import is a statement: no expression holds one, so the walk stops at the statements
      if isinstance(child, (ast.Import, ast.ImportFrom)):
        if in_function:
          found.append(child)
      else:
        visit(child, in_function=in_function or isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)))

  visit(tree, in_function=False)
  return found


def comments_by_line(source: str) -> dict[int, tuple[str, bool]]:
  """Line number -> (comment text, whether the comment is alone on its line), for each line that holds a comment."""
  return {
      token.start[0]: (token.string, token.line.lstrip().startswith("#"))
      for token in tokenize.generate_tokens(io.StringIO(source).readline)
      if token.type == tokenize.COMMENT
  }


def has_deferral(comments: dict[int, tuple[str, bool]], line: int) -> bool:
  """Whether ``# deferred: <text>`` ends ``line`` or stands alone on the line directly above it."""
  for number, alone in ((line, False), (line - 1, True)):
    found = comments.get(number)
    if found is not None and found[1] == alone and DEFERRAL_COMMENT.match(found[0]):
      return True
  return False


def deferral_breaks(root: Path) -> list[str]:
  """One failure line per function-level import of a src module, under src/runtime or src/infra, that has no
  ``# deferred: <command>`` comment. The target resolves as in :func:`scan`."""
  index = module_index(root)
  failures = []
  for rel in index.values():
    if not rel.startswith(DEFERRAL_SCOPES):
      continue
    source = (root / rel).read_text(encoding="utf-8")
    package = ".".join(Path(rel).with_suffix("").parts[:-1])
    comments = None
    for node in function_level_imports(ast.parse(source, filename=rel)):
      targets = {
          target for name in named_modules(node, package, index) if (target := resolve(name, index)) not in (None, rel)
      }
      if not targets:
        continue
      if comments is None:
        comments = comments_by_line(source)  # tokenizing is slow: only a file with a src import needs it
      if not has_deferral(comments, node.lineno):
        failures.append(
            f"{rel}:{node.lineno}: function-level import of {', '.join(sorted(targets))} has no "
            "'# deferred: <command>' comment; move it to the module top, or name the command whose startup it protects."
        )
  return failures


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

# The synthetic tree of the layer test, under the table SMALL_LAYERS. bottom.py (layer 1) imports top (layer 3)
# four ways: at module level, in a `TYPE_CHECKING` block, in a function body and as a module-path string. It also
# imports the marker of the layer-3 package `upper`, which passes. top.py imports ring_a (layer 2), which passes.
# ring_a.py and ring_b.py (layer 2) import each other, once at module level and once in a function body. The marker
# of `upper` and upper/view.py import each other, which no check counts. left.py and right.py of src/infra import
# each other. unlisted.py is in no layer, and the table lists `gone`, which no module matches.
SMALL_LAYERS = (
    Layer(3, "top", ("top", "upper")),
    Layer(2, "middle", ("ring_a", "ring_b")),
    Layer(1, "bottom", ("bottom", "gone")),
)
LAYERED_MODULES = {
    "server.py": "",
    "src/__init__.py": "",
    "src/runtime/__init__.py": "",
    "src/runtime/unlisted.py": "",
    "src/runtime/top.py": "from src.runtime import ring_a\n",
    "src/runtime/ring_a.py": "from src.runtime import ring_b\n",
    "src/runtime/ring_b.py": "def back():\n  from src.runtime import ring_a\n",
    "src/runtime/upper/__init__.py": "from src.runtime.upper import view\n",
    "src/runtime/upper/view.py": "from src.runtime.upper import VIEWS\n",
    "src/infra/__init__.py": "",
    "src/infra/left.py": "from src.infra import right\n",
    "src/infra/right.py": "from src.infra import left\n",
}
BOTTOM_SOURCE = textwrap.dedent(
    """\
    from typing import TYPE_CHECKING

    from src.runtime import top
    from src.runtime.upper import VIEWS
    if TYPE_CHECKING:
      from src.runtime.top import Thing


    def later():
      from src.runtime import top as inner
      return "src.runtime.top"
    """)

# The synthetic tree of the deferral test. Each module of src/runtime holds one function-level import: marked.py
# imports src/infra/models.py twice with a comment (at the end of the line, and alone on the line above); unmarked.py
# and empty.py have no comment and an empty one; outside.py imports only the standard library and a third-party module.
DEFERRAL_MODULES = {
    "server.py": "",
    "src/__init__.py": "",
    "src/infra/__init__.py": "",
    "src/infra/models.py": "",
    "src/runtime/__init__.py": "",
    "src/runtime/marked.py":
        (
            "def at_the_end():\n  from src.infra import models  # deferred: charliebot improve --help\n\n\n"
            "def alone_above():\n  # deferred: charliebot improve --help\n  from src.infra import models\n"),
    "src/runtime/unmarked.py": "def bare():\n  from src.infra import models\n",
    "src/runtime/empty.py": "def blank():\n  from src.infra import models  # deferred:\n",
    "src/runtime/outside.py": "def later():\n  import json\n  import yaml\n  from collections import OrderedDict\n",
}


def write_tree(root: Path, modules: dict[str, str]) -> None:
  for rel, source in modules.items():
    (root / rel).parent.mkdir(parents=True, exist_ok=True)
    (root / rel).write_text(source, encoding="utf-8")


@pytest.fixture(scope="module")
def structure() -> Structure:
  return scan(conftest.ROOT)


@pytest.fixture(scope="module")
def runtime_paths() -> list[str]:
  return runtime_files(conftest.ROOT)


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


def test_no_runtime_module_imports_a_higher_layer(structure: Structure) -> None:
  failures = layer_breaks(structure, RUNTIME_LAYERS)
  assert not failures, "\n".join(failures)


def test_every_runtime_module_has_a_layer(runtime_paths: list[str]) -> None:
  failures = unplaced_modules(runtime_paths, RUNTIME_LAYERS)
  assert not failures, "\n".join(failures)


def test_every_layer_entry_names_a_runtime_module(runtime_paths: list[str]) -> None:
  failures = stale_entries(runtime_paths, RUNTIME_LAYERS)
  assert not failures, "\n".join(failures)


@pytest.mark.parametrize("package", ["src/runtime/", "src/infra/"], ids=["runtime", "infra"])
def test_the_import_graph_has_no_cycle(structure: Structure, package: str) -> None:
  failures = import_cycles(structure, package)
  assert not failures, "\n\n".join(failures)


def test_every_function_level_import_names_its_command() -> None:
  failures = deferral_breaks(conftest.ROOT)
  assert not failures, "\n".join(failures)


def test_the_scan_counts_every_form_of_import(tmp_path: Path) -> None:
  """An import form the scan skips would let a reverse import in unseen."""
  modules = dict.fromkeys(EMPTY_MODULES, "")
  modules["src/runtime/core.py"] = FORMS_SOURCE
  write_tree(tmp_path, modules)

  found = scan(tmp_path)

  assert found.ungrouped == []
  assert {(r.importer, r.line, r.target) for r in found.references} == (
      {("src/runtime/core.py", line, "src/features/alpha/impl.py") for line in (3, 5, 9, 10, 11, 12, 13)} |
      {("src/runtime/core.py", line, "src/features/__init__.py") for line in (14, 15)})
  assert [built.line for built in found.built_paths] == [14, 15]
  assert {(r.importer, r.target) for r in violations(found)} == {("src/runtime/core.py", "src/features/alpha/impl.py")}


def test_the_layer_checks_find_each_break(tmp_path: Path) -> None:
  """A form of import the checks skip would let an upward import in unseen, and a package marker they count would
  fail a clean tree."""
  write_tree(tmp_path, {**LAYERED_MODULES, "src/runtime/bottom.py": BOTTOM_SOURCE})

  found = scan(tmp_path)
  files = runtime_files(tmp_path)

  def subjects(failures: list[str]) -> list[str]:
    return [failure.partition(": ")[0] for failure in failures]

  assert found.ungrouped == []
  assert subjects(layer_breaks(found, SMALL_LAYERS)) == [f"src/runtime/bottom.py:{line}" for line in (3, 6, 10, 11)]
  assert subjects(unplaced_modules(files, SMALL_LAYERS)) == ["src/runtime/unlisted.py"]
  assert subjects(stale_entries(files, SMALL_LAYERS)) == ["gone"]
  [runtime_cycle] = import_cycles(found, "src/runtime/")
  runtime_header, *runtime_edges = runtime_cycle.splitlines()
  assert runtime_header.endswith("src/runtime/ring_a.py, src/runtime/ring_b.py")
  assert runtime_edges == [
      "  src/runtime/ring_a.py:1 -> src/runtime/ring_b.py", "  src/runtime/ring_b.py:2 -> src/runtime/ring_a.py"
  ]
  [infra_cycle] = import_cycles(found, "src/infra/")
  infra_header, *infra_edges = infra_cycle.splitlines()
  assert infra_header.endswith("src/infra/left.py, src/infra/right.py")
  assert infra_edges == ["  src/infra/left.py:1 -> src/infra/right.py", "  src/infra/right.py:1 -> src/infra/left.py"]
  with pytest.raises(ValueError, match="top"):
    layer_of("top", (Layer(2, "first", ("top",)), Layer(1, "second", ("top",))))


def test_the_deferral_check_finds_each_unmarked_import(tmp_path: Path) -> None:
  """A deferral without its command, or with an empty one, would stay in the code unread."""
  write_tree(tmp_path, DEFERRAL_MODULES)

  failures = deferral_breaks(tmp_path)

  assert failures == [
      f"src/runtime/{name}.py:2: function-level import of src/infra/models.py has no '# deferred: <command>' comment; "
      "move it to the module top, or name the command whose startup it protects." for name in ("empty", "unmarked")
  ]
