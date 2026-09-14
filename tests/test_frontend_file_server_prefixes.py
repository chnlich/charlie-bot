"""The file-server prefix set: one served truth, two mirrors, pinned equal.

The chat link normalizer (web/static/js/chat/artifacts.js) repairs same-host links whose
scheme or port was written from memory only for paths under a file-server prefix, because
those are routes this server itself answers: server.py mounts the one files router under
every prefix of FILE_SERVER_MOUNTS (src/core/constants.py). The frontend cannot import
that tuple, so its gate array and an older chat test's PREFIXES literal mirror the set.
This test fails the merge in which any operand drifts: a prefix renamed in the home
without updating the mirrors, or a mirror edited alone.

All parses anchor on the declaration line (assignment), never on a bare prefix string:
the prefix strings recur in comments and probe URLs nearby, and an occurrence scan would
forgive a missing declaration.
"""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

_MIRRORS = {
    "web/static/js/chat/artifacts.js": re.compile(r"FILE_SERVER_PREFIXES\s*=\s*\[([^]]*)\]"),
    "tests/chat_file_link_prefixes.test.js": re.compile(r"PREFIXES\s*=\s*\[([^]]*)\]"),
}

_STRING_RE = re.compile(r"'([^']+)'|\"([^\"]+)\"")


def _served_prefixes() -> set[str]:
  text = (ROOT / "src/core/constants.py").read_text()
  match = re.search(r"FILE_SERVER_MOUNTS\s*=\s*\(([^)]*)\)", text)
  assert match, "FILE_SERVER_MOUNTS declaration not found in src/core/constants.py"
  return {single or double for single, double in _STRING_RE.findall(match.group(1))}


def _mirror_prefixes(rel: str, pattern: re.Pattern) -> set[str]:
  text = (ROOT / rel).read_text()
  match = pattern.search(text)
  assert match, f"{rel}: prefix declaration not found"
  return {single or double for single, double in _STRING_RE.findall(match.group(1))}


def test_file_server_prefixes_single_set() -> None:
  served = _served_prefixes()
  assert len(served) > 1, f"FILE_SERVER_MOUNTS parsed as {served}; expected both prefixes"
  for rel, pattern in _MIRRORS.items():
    mirror = _mirror_prefixes(rel, pattern)
    assert mirror == served, f"{rel} declares {mirror}, constants home declares {served}"
