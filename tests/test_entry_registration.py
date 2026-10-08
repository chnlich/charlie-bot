"""Every entry outside src that parses config registers the packages first.

Config parsing reads package sections and backend option models from a registry that
``registrations.register_all()`` fills (src/infra/config_registry.py). A process that parses the
real config without that call refuses its ``accounts`` section and its backend options as unknown
keys. A file under scripts/, tools/ or skills/ that calls ``get_config``, ``load_config`` or
``CharlieBotConfig`` therefore also calls ``register_all``. A shell script is scanned through its
Python heredocs, each of which is one process.
"""

from __future__ import annotations

import ast
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCAN_DIRS = ("scripts", "tools", "skills")
CONFIG_PARSERS = {"get_config", "load_config", "CharlieBotConfig"}
REGISTRATION = "register_all"

# Repo-relative path -> why the file parses config without register_all().
EXEMPT: dict[str, str] = {}

HEREDOC_OPEN = re.compile(r"<<-?\s*(['\"]?)(\w+)\1")
PYTHON_COMMAND = re.compile(r"\bpython3?\b")


def _python_heredocs(shell_source: str) -> list[str]:
  """The bodies of the heredocs whose command line runs python, in file order."""
  bodies: list[str] = []
  lines = iter(shell_source.splitlines())
  for line in lines:
    opened = HEREDOC_OPEN.search(line)
    if opened is None:
      continue
    body: list[str] = []
    for body_line in lines:
      if body_line.strip() == opened.group(2):
        break
      body.append(body_line)
    if PYTHON_COMMAND.search(line):
      bodies.append("\n".join(body))
  return bodies


def _called_names(python_source: str, label: str) -> set[str]:
  names: set[str] = set()
  for node in ast.walk(ast.parse(python_source, filename=label)):
    if isinstance(node, ast.Call):
      func = node.func
      if isinstance(func, ast.Name):
        names.add(func.id)
      elif isinstance(func, ast.Attribute):
        names.add(func.attr)
  return names


def _scanned_processes() -> list[tuple[str, str, set[str]]]:
  """(file, label, called names) per Python process: one per .py file, one per Python heredoc in a .sh file."""
  processes: list[tuple[str, str, set[str]]] = []
  for scan_dir in SCAN_DIRS:
    for path in sorted((ROOT / scan_dir).rglob("*")):
      relative = path.relative_to(ROOT).as_posix()
      if path.suffix == ".py":
        processes.append((relative, relative, _called_names(path.read_text(encoding="utf-8"), relative)))
      elif path.suffix == ".sh":
        for index, body in enumerate(_python_heredocs(path.read_text(encoding="utf-8")), start=1):
          label = f"{relative} (python heredoc {index})"
          processes.append((relative, label, _called_names(body, label)))
  return processes


def test_entries_outside_src_that_parse_config_call_register_all() -> None:
  parsing = [(file, label, names) for file, label, names in _scanned_processes() if names & CONFIG_PARSERS]
  assert parsing, "the scan found no config-parsing entry: its directories or patterns drifted"

  unregistered = [label for file, label, names in parsing if REGISTRATION not in names and file not in EXEMPT]

  assert not unregistered, (
      f"these entries parse config without calling {REGISTRATION}(); call it first "
      "(from src.app.registrations), or list the file in EXEMPT with the reason:\n" +
      "\n".join(f"  {label}" for label in unregistered))
