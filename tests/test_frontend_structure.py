"""Frontend structure check: the groups of the checked pages and the names each group may use.

Every frontend file the checked pages load belongs to one group:

    core            every file under web/
    features.<x>    src/features/<x>/static/** and src/features/<x>/templates/<x>/page_*.html
    backends.<x>    src/backends/<x>/static/** and src/backends/<x>/templates/<x>/page_*.html

The chat page is web/templates/index.html plus every package's page part templates. The page loads its
files in layers, and the load order is
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

web/templates/index.html loads core in five layers, lowest first:

    base        platform.js to chat/shared.js: the helpers, config.js and the Chat and Sidebar namespaces
    connection  websocket.js and tabs.js
    chat view   chat/ratings.js to file-upload.js
    sidebar     sidebar/row-menu.js to sidebar/session-menus.js
    control     chat/input.js, websocket-handlers.js and app.js

A package learns core's facts through a registration entry: a function core provides, and the
package's script registers a handler with it at load time. The core registration entries:

    Chat.registerDeliveryLink   web/static/js/chat/rendering.js (a link on the worker delivery banner)
    Chat.registerFileLinkFamily web/static/js/chat/file-links.js (a family of file links the package owns)
    Chat.registerForYouRules    web/static/js/chat/turn-engine.js (the For-you projection's rules, from chat/for-you.js)
    Chat.registerInputMode      web/static/js/chat/attachments.js (an input mode and its user-bubble badge)
    Chat.registerMessagePostProcessor
                                web/static/js/chat/rendering.js (a pass over each rendered message root)
    Chat.registerProjectionCheck
                                web/static/js/chat/turn-engine.js (the page's check, run after each projection that ran)
    Chat.registerSeparatorButton
                                web/static/js/chat/rendering.js (a button on each separator line)
    Chat.registerSeparatorPanel web/static/js/chat/turn-engine.js (a panel under a separator line that the engine reopens)
    registerGroupMenuItem       web/static/js/sidebar/groups.js (an item of a group header's menu)
    registerSessionMenuItems    web/static/js/sidebar/groups.js (items of a session row's Settings menu)
    registerSessionRowBadge     web/static/js/sidebar/groups.js (a badge after a session row's indicators)
    registerSessionRowLine      web/static/js/sidebar/groups.js (a line in place of a session row's time line)
    registerSidebarControl      web/static/js/sidebar/namespace.js (the handler a rendered sidebar control calls by name)
    registerSidebarFilter       web/static/js/sidebar/filters.js (a sidebar filter button)
    registerTab                 web/static/js/tabs.js (a tab: its layout, show and hide callbacks)
    registerWorkspaceBadge      web/static/js/sidebar/filters.js (a badge at the top of the Workspace list)
    registerWsConnectHandler    web/static/js/websocket.js (a handler of every socket (re)connect)
    registerWsMessageHandler    web/static/js/websocket.js (the handler of one WebSocket event type)

A package that adds an item to a core menu needs no registration entry: it appends its own
element to the menu's container (the voice package adds the composer ⋯ menu's Voice backend
entry to #overflow-menu), so the core markup names nothing the package defines.

Core announces a moment that needs no answer as a CustomEvent on document, and a package listens
for it. The core page events:

    charliebot:active-session-changed
                                web/static/js/sidebar/session-view.js (SESSION_ID names another session)
    charliebot:session-data-load
                                web/static/js/sidebar/session-view.js (the idle load of the session's per-session data)
    charliebot:session-switch-phase
                                web/static/js/sidebar/session-view.js (one phase of a session switch, with its timing)
    charliebot:session-switched web/static/js/sidebar/session-view.js (the end of a session switch)
    charliebot:session-view-teardown
                                web/static/js/sidebar/session-view.js (the active session view is torn down)

A landing that adds a registration entry or a page event extends these lists.

An upward use — a file using a name that only a higher file defines — has three fixes. Move the
shared constant or function to the lower file. The higher file registers a handler through a
registration entry of the lower file. The higher file attaches a delegated DOM listener in place
of a generated onclick string that names a higher function.
"""

import json
import pathlib
import re
import shutil
import subprocess
from collections.abc import Callable

import conftest
import pytest
import test_package_structure

GRAPH_SCRIPT = pathlib.Path(__file__).with_name("frontend_structure_graph.js")
PAGE_NAMES = ("chat",)

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


def direction_breaks(pages: dict[str, dict]) -> dict[str, dict[tuple[str, str], set[str]]]:
  """The (user, definer) pairs that break a direction, per rule, with the used names per pair. A node
  in no group is left to the group check."""
  breaks: dict[str, dict[tuple[str, str], set[str]]] = {
      "core-uses-package": {},
      "package-uses-other-group": {},
      "core-loads-late": {}
  }
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


def break_failures(breaks: dict[tuple[str, str], set[str]], message: Callable[[str, str, str], str]) -> list[str]:
  """One failure line per violating pair."""
  return [message(user, definer, ", ".join(sorted(names))) for (user, definer), names in sorted(breaks.items())]


@pytest.mark.integration
def test_frontend_files_have_groups_and_unique_global_names(page_graphs: dict[str, dict]) -> None:
  failures = [
      f"{node}: outside the groups web/ (core) and src/<kind>/<name> (package); move it into one."
      for page_name in PAGE_NAMES
      for node in page_graphs[page_name]["nodes"]
      if group_of(node) is None
  ]
  failures.extend(
      f"{name}: defined by {', '.join(nodes)}; one global name is defined in one file only." for page_name in PAGE_NAMES
      for name, nodes in sorted(page_graphs[page_name]["multiDefs"].items()))
  assert not failures, "\n".join(failures)


@pytest.mark.integration
def test_core_uses_no_name_a_package_defines(page_graphs: dict[str, dict]) -> None:
  failures = break_failures(
      direction_breaks(page_graphs)["core-uses-package"], lambda user, definer, names:
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

  failures = break_failures(direction_breaks(page_graphs)["package-uses-other-group"], message)
  assert not failures, "\n".join(failures)


@pytest.mark.integration
def test_a_core_file_uses_only_core_names_defined_before_it_loads(page_graphs: dict[str, dict]) -> None:
  failures = break_failures(
      direction_breaks(page_graphs)["core-loads-late"], lambda user, definer, names:
      f"{user} uses {names}, defined by {definer}, which loads later; a core file uses only names "
      f"that core files loaded before it define. {FIX_THE_UPWARD_USE}")
  assert not failures, "\n".join(failures)


def test_web_static_has_no_package_static_directory() -> None:
  failures = [
      f"web/static/{kind}/: this URL prefix belongs to package static files, served from "
      f"src/{kind}/<name>/static/; remove the directory."
      for kind in ("features", "backends")
      if (conftest.ROOT / "web" / "static" / kind).exists()
  ]
  assert not failures, "\n".join(failures)
