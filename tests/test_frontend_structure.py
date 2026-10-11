"""Frontend structure check: the groups of the checked pages and the names each group may use.

Every frontend file the checked pages load belongs to one group:

    core            every file under web/
    features.<x>    src/features/<x>/static/** and src/features/<x>/templates/<x>/page_*.html
    backends.<x>    src/backends/<x>/static/** and src/backends/<x>/templates/<x>/page_*.html

The chat page is web/templates/index.html plus every package's page part templates. The diff page
is src/features/diff_view/templates/diff.html plus the packages' page_diff_toolbar.html parts. The
artifact page's script list is the <script src=...> strings in
src/features/artifacts/artifact_view.py. Each page loads its files in layers, and the load order is
the layer order: the inline <script> bodies of a template are its lowest node (bootstrap), each
script its <script src="/static/..."> tags load is one node, and its inline handler attributes
(onclick="..." and the like) are its highest node (markup). Package parts load after core, in
src/app/registrations.py order.

Directions: core uses no name a package defines. A package uses names of core and of itself only;
the features in CHAT_CHANNELS may also use features.chat_threads, and the backends in
BACKEND_VARIANTS may also use backends.claude_code. A core node uses only names that core nodes
loaded before it define, so core has no cycle between files. A cycle inside one package is allowed.
One global name is defined in one file only. web/static has no features/ and no backends/
directory: those URL prefixes belong to package static files.

A package learns core's facts through a registration entry: a function core provides, and the
package's script registers a handler with it at load time. The core registration entries today:

    registerSidebarFilter    web/static/js/sidebar/filters.js (a sidebar filter button)

A landing that adds a registration entry extends this list.

An upward use — a file using a name that only a higher file defines — has three fixes. Move the
shared constant or function to the lower file. The higher file registers a handler through a
registration entry of the lower file. The higher file attaches a delegated DOM listener in place
of a generated onclick string that names a higher function.

frontend_structure_exceptions.txt records the upward uses that predate this structure: one row per
user file -> defining file pair, repo-relative and sorted. A violating pair absent from the table
fails the check, and a row that no longer violates fails it, so the table only shrinks. The last
landing of the frontend restructure deletes the table.
"""

import json
import re
import shutil
import subprocess
from collections.abc import Callable
from pathlib import Path

import conftest
import pytest
import test_package_structure

GRAPH_SCRIPT = Path(__file__).with_name("frontend_structure_graph.js")
EXCEPTIONS_PATH = Path(__file__).with_name("frontend_structure_exceptions.txt")
PAGE_NAMES = ("chat", "diff", "artifact")

FIX_THE_UPWARD_USE = (
    "Fix it in one of three ways. Move the shared constant or function to the lower file. The higher file "
    "registers a handler through a registration entry of the lower file. The higher file attaches a delegated "
    "DOM listener in place of a generated onclick string that names a higher function.")

# A package part: a static file of the package, or one of its templates/<name>/page_*.html part
# templates. A package that adds files at these paths joins the check without an edit here.
_PACKAGE_PART = re.compile(
    r"src/(features|backends)/([A-Za-z0-9_]+)/(?:static/.+|templates/([A-Za-z0-9_]+)/page_[^/]*\.html)")


def group_of(node: str) -> str | None:
  """The group of one graph node: core for a file under web/, <kind>.<name> for a package part; None
  when the node's path is in no group."""
  path = node.partition("#")[0]  # strip the #bootstrap or #markup of a template node
  if path.startswith("web/"):
    return "core"
  match = _PACKAGE_PART.fullmatch(path)
  if match is None or (match.group(3) is not None and match.group(3) != match.group(2)):
    return None
  return f"{match.group(1)}.{match.group(2)}"


def allowed_groups(group: str) -> set[str]:
  """The groups whose names a package may use: core, the package itself, and its one exception."""
  kind, _, name = group.partition(".")
  allowed = {"core", group}
  if kind == "features" and name in test_package_structure.CHAT_CHANNELS:
    allowed.add("features.chat_threads")
  if kind == "backends" and name in test_package_structure.BACKEND_VARIANTS:
    allowed.add("backends.claude_code")
  return allowed


def run_checked(cmd: list[str]) -> subprocess.CompletedProcess[str]:
  """One subprocess of the graph build; a nonzero exit fails with the child's output, in the shape of
  tests/test_frontend_js.py."""
  result = subprocess.run(cmd, cwd=conftest.ROOT, capture_output=True, text=True, check=False, timeout=300)
  if result.returncode != 0:
    pytest.fail(f"{' '.join(cmd)} failed.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}")
  return result


@pytest.fixture(scope="module")
def page_graphs() -> dict[str, dict]:
  """The dependency graph of the three pages, computed once per module: npm install (the same
  idempotent step tests/tailwind_class_coverage.test.js runs), then the node graph script."""
  node = shutil.which("node")
  if node is None:
    pytest.skip("node is required for the frontend structure test")
  run_checked(["npm", "install", "--no-audit", "--no-fund"])
  result = run_checked([node, str(GRAPH_SCRIPT), str(conftest.ROOT)])
  return json.loads(result.stdout)["pages"]


def recorded_pairs() -> set[tuple[str, str]]:
  """The (user, definer) rows of the exceptions table."""
  pairs = set()
  for line in EXCEPTIONS_PATH.read_text(encoding="utf-8").splitlines():
    if line.strip() and not line.startswith("#"):
      user, _, definer = line.partition(" -> ")
      pairs.add((user.strip(), definer.strip()))
  return pairs


def direction_breaks(pages: dict[str, dict]) -> dict[str, dict[tuple[str, str], set[str]]]:
  """The (user, definer) pairs that break a direction, per rule, with the used names per pair. A node
  in no group is left to the group check."""
  breaks: dict[str, dict[tuple[str, str], set[str]]] = {
      "core-uses-package": {}, "package-uses-other-group": {}, "core-loads-late": {}}
  for page_name in PAGE_NAMES:
    page = pages[page_name]
    position = {node: index for index, node in enumerate(page["nodes"])}
    groups = {node: group_of(node) for node in page["nodes"]}
    for user, targets in page["edges"].items():
      for definer, names in targets.items():
        user_group, definer_group = groups[user], groups[definer]
        if user_group is None or definer_group is None:
          continue
        if user_group == "core" and definer_group != "core":
          key = "core-uses-package"
        elif user_group != "core" and definer_group not in allowed_groups(user_group):
          key = "package-uses-other-group"
        elif user_group == "core" and definer_group == "core" and position[definer] > position[user]:
          key = "core-loads-late"
        else:
          continue
        breaks[key].setdefault((user, definer), set()).update(names)
  return breaks


def unrecorded_failures(breaks: dict[tuple[str, str], set[str]], recorded: set[tuple[str, str]],
                        message: Callable[[str, str, str], str]) -> list[str]:
  """One failure line per violating pair the exceptions table does not record."""
  return [
      message(user, definer, ", ".join(sorted(names)))
      for (user, definer), names in sorted(breaks.items()) if (user, definer) not in recorded
  ]


@pytest.mark.integration
def test_every_frontend_file_is_in_a_group(page_graphs: dict[str, dict]) -> None:
  failures = [
      f"{node}: outside the groups web/ (core) and src/<kind>/<name> (package); move it into one."
      for page_name in PAGE_NAMES for node in page_graphs[page_name]["nodes"] if group_of(node) is None
  ]
  assert not failures, "\n".join(failures)


@pytest.mark.integration
def test_core_uses_no_name_a_package_defines(page_graphs: dict[str, dict]) -> None:
  failures = unrecorded_failures(
      direction_breaks(page_graphs)["core-uses-package"], recorded_pairs(),
      lambda user, definer, names:
      f"{user} uses {names}, defined by {definer} (group {group_of(definer)}); core uses no name a "
      f"package defines. {FIX_THE_UPWARD_USE}")
  assert not failures, "\n".join(failures)


@pytest.mark.integration
def test_a_package_uses_only_core_itself_and_its_exception(page_graphs: dict[str, dict]) -> None:
  def message(user: str, definer: str, names: str) -> str:
    may = ", ".join(sorted(allowed_groups(group_of(user))))
    return (
        f"{user} (group {group_of(user)}) uses {names}, defined by {definer} "
        f"(group {group_of(definer)}); {group_of(user)} may use names of {may} only.")

  failures = unrecorded_failures(
      direction_breaks(page_graphs)["package-uses-other-group"], recorded_pairs(), message)
  assert not failures, "\n".join(failures)


@pytest.mark.integration
def test_a_core_file_uses_only_core_names_defined_before_it_loads(page_graphs: dict[str, dict]) -> None:
  failures = unrecorded_failures(
      direction_breaks(page_graphs)["core-loads-late"], recorded_pairs(),
      lambda user, definer, names:
      f"{user} uses {names}, defined by {definer}, which loads later; a core file uses only names "
      f"that core files loaded before it define. {FIX_THE_UPWARD_USE}")
  assert not failures, "\n".join(failures)


@pytest.mark.integration
def test_one_global_name_is_defined_in_one_file_only(page_graphs: dict[str, dict]) -> None:
  failures = [
      f"{name}: defined by {', '.join(nodes)}; one global name is defined in one file only."
      for page_name in PAGE_NAMES for name, nodes in sorted(page_graphs[page_name]["multiDefs"].items())
  ]
  assert not failures, "\n".join(failures)


def test_web_static_has_no_package_static_directory() -> None:
  failures = [
      f"web/static/{kind}/: this URL prefix belongs to package static files, served from "
      f"src/{kind}/<name>/static/; remove the directory."
      for kind in ("features", "backends") if (conftest.ROOT / "web" / "static" / kind).exists()
  ]
  assert not failures, "\n".join(failures)


@pytest.mark.integration
def test_every_recorded_upward_use_still_breaks_a_direction(page_graphs: dict[str, dict]) -> None:
  current = {pair for rule_breaks in direction_breaks(page_graphs).values() for pair in rule_breaks}
  failures = [
      f"{user} -> {definer}: this use is gone, or no longer breaks a direction; remove its row."
      for user, definer in sorted(recorded_pairs() - current)
  ]
  assert not failures, "\n".join(failures)
