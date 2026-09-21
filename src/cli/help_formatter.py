"""The CLI's shared argparse help formatter.

``argparse.HelpFormatter.__init__`` resolves the terminal width through
``shutil.get_terminal_size`` when *width* is None, and that lazy import pulls
``bz2`` and ``lzma`` with it (~2.4 ms measured on this host) into every
fresh-process CLI verb — the verbs build their parser in every process. The
subclass resolves the width itself under shutil.get_terminal_size's documented
precedence (``COLUMNS`` env, then the stdout terminal, then 80), so the
formatted help stays byte-identical while the verb's parser build imports
neither shutil nor its archive backends.
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


class CliHelpFormatter(argparse.HelpFormatter):
  """HelpFormatter whose width never imports shutil."""

  def __init__(
      self, prog: str, indent_increment: int = 2, max_help_position: int = 24, width: int | None = None) -> None:
    if width is None:
      width = _terminal_columns() - 2
    super().__init__(prog, indent_increment, max_help_position, width)


class CliRawDescriptionHelpFormatter(CliHelpFormatter, argparse.RawDescriptionHelpFormatter):
  """The RawDescriptionHelpFormatter shape (epilogs render verbatim) on the shutil-free width."""
