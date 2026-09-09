"""The file-server prefix set: one served truth, three mirrors, pinned equal.

The chat link normalizer (web/static/js/chat/artifacts.js) repairs same-host links whose
scheme or port was written from memory only for paths under a file-server prefix, because
those are routes this server itself answers: server.py mounts the one files router under
every served prefix. Three other declarations mirror that set: the frontend gate array,
the pages tuple, and an older chat test's PREFIXES literal. This test fails the merge in
which any operand drifts: a prefix mounted server-side without updating the mirrors, or a
mirror edited alone.

Both parses anchor on the declaration line (assignment or the files.router include), never
on a bare prefix string: the prefix strings recur in comments and probe URLs nearby, and an
occurrence scan would forgive a missing declaration.
"""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

_SERVERSIDE_INCLUDE_RE = re.compile(r'include_router\(files\.router,\s*prefix="([^"]+)"')

_MIRRORS = {
    "web/static/js/chat/artifacts.js": re.compile(r"FILE_SERVER_PREFIXES\s*=\s*\[([^]]*)\]"),
    "src/api/pages.py": re.compile(r"_FILE_SERVER_PREFIXES\s*=\s*\(([^)]*)\)"),
    "tests/chat_file_link_prefixes.test.js": re.compile(r"PREFIXES\s*=\s*\[([^]]*)\]"),
}

_STRING_RE = re.compile(r"'([^']+)'|\"([^\"]+)\"")


def _served_prefixes() -> set[str]:
  text = (ROOT / "server.py").read_text()
  return set(_SERVERSIDE_INCLUDE_RE.findall(text))


def _mirror_prefixes(rel: str, pattern: re.Pattern) -> set[str]:
  text = (ROOT / rel).read_text()
  match = pattern.search(text)
  assert match, f"{rel}: prefix declaration not found"
  return {single or double for single, double in _STRING_RE.findall(match.group(1))}


def test_file_server_prefixes_single_set() -> None:
  served = _served_prefixes()
  assert len(served) > 1, f"server.py files.router mounts parsed as {served}; expected both prefixes"
  for rel, pattern in _MIRRORS.items():
    mirror = _mirror_prefixes(rel, pattern)
    assert mirror == served, f"{rel} declares {mirror}, server serves {served}"
