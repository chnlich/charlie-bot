"""The CLI's shared argparse help formatter.

``argparse.HelpFormatter.__init__`` resolves the terminal width through
``shutil.get_terminal_size`` when *width* is None, and that lazy import pulls
``bz2`` and ``lzma`` with it (~2.4 ms measured on this host) into every
fresh-process CLI verb — the verbs build their parser in every process. The
subclass resolves the width itself under shutil.get_terminal_size's documented
precedence (``COLUMNS`` env, then the stdout terminal, then 80), so the
formatted help stays byte-identical while the verb's parser build imports
neither shutil nor its archive backends.

The subclass also keeps ``_colorize`` (stdlib, ~12 ms with its dataclasses +
inspect chain) out of the piped verb: argparse's first formatter construction
calls ``_set_color``, whose module-level ``from _colorize import ...`` prices
every ``charliebot`` invocation. The override reproduces the two arms exactly:
the colorized arm delegates to the stock method (the real import, the real
decision), and the no-color arm installs the empty theme without importing —
the stock no-color theme is every style field set to ``""``, so any attribute
read renders empty in both. See ``_can_colorize`` for the one mirror whose
drift is possible and what it costs.
"""

from __future__ import annotations

import argparse
import os
import sys


def _terminal_columns() -> int:
  """The stdout terminal's column count under shutil.get_terminal_size's precedence."""
  try:
    columns = int(os.environ["COLUMNS"])
  except (KeyError, ValueError):
    columns = 0
  if columns > 0:
    return columns
  try:
    return os.get_terminal_size(sys.__stdout__.fileno()).columns
  except (AttributeError, ValueError, OSError):
    return 80


def _can_colorize() -> bool:
  """Mirror of ``_colorize.can_colorize(file=sys.stdout)`` on POSIX.

  The mirror exists so the piped arm never pays the module import; the
  colorized arm re-decides through the stock method, so a false "yes" only
  costs the import, and a false "no" only drops color in an exotic
  environment — bytes stay correct either way. Windows is excluded because
  stdlib's decision there reads the console's virtual-terminal state.
  """
  env = os.environ
  if not sys.flags.ignore_environment:
    if env.get("PYTHON_COLORS") == "0":
      return False
    if env.get("PYTHON_COLORS") == "1":
      return True
  if env.get("NO_COLOR"):
    return False
  if env.get("FORCE_COLOR"):
    return True
  if env.get("TERM") == "dumb":
    return False
  try:
    return os.isatty(sys.stdout.fileno())
  except OSError:
    return hasattr(sys.stdout, "isatty") and sys.stdout.isatty()


class _NoColorTheme:
  """Stand-in for ``Argparse.no_colors()``: every style attribute reads empty."""

  def __getattr__(self, name: str) -> str:
    if name.startswith("__") and name.endswith("__"):
      raise AttributeError(name)
    return ""


def _identity(value):
  return value


class CliHelpFormatter(argparse.HelpFormatter):
  """HelpFormatter whose width never imports shutil and whose piped arm never imports _colorize."""

  def __init__(
      self, prog: str, indent_increment: int = 2, max_help_position: int = 24, width: int | None = None) -> None:
    if width is None:
      width = _terminal_columns() - 2
    super().__init__(prog, indent_increment, max_help_position, width)

  def _set_color(self, color: bool) -> None:
    if color and _can_colorize():
      super()._set_color(color)
      return
    self._theme = _NoColorTheme()
    self._decolor = _identity


class CliRawDescriptionHelpFormatter(CliHelpFormatter, argparse.RawDescriptionHelpFormatter):
  """The RawDescriptionHelpFormatter shape (epilogs render verbatim) on the shutil-free width."""
