"""Claude TUI pane parsing for claude-sub."""

from __future__ import annotations

import re
from dataclasses import dataclass
from enum import IntEnum
from typing import Optional


class PaneInputKind(IntEnum):
  UNKNOWN = 0
  PROMPT = 1
  MENU = 2


@dataclass(frozen=True)
class PaneInputState:
  kind: PaneInputKind
  content: str = ""


# Selection cursor (❯) sitting on a numbered option, as rendered by AskUserQuestion
# / plan-approval / permission menus. The idle input prompt also starts with ❯ but
# is never followed by a digit, so requiring a number distinguishes a blocking menu.
# Must only ever run on dim-stripped real content: ghost suggestions render dim and
# may start with a digit. Submitted prompts are echoed into scrollback with the same
# cursor prefix, so context decides whether a digit-leading cursor line is prompt
# text or a blocking menu.
_MENU_OPTION_RE = re.compile(r"❯\s*\d")

# SGR sequence (CSI ... m); group 1 holds the parameter list that toggles dim.
_SGR_RE = re.compile(r"\x1b\[([0-9;]*)m")
# Any other CSI sequence, stripped without affecting dim state.
_CSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")


def strip_dim_text(line: str) -> str:
  """Return only the characters of `line` rendered with SGR dim OFF, with escape codes removed.

  Ghost suggestions and idle hints in the claude TUI input box render SGR dim and are
  indistinguishable from real input in a plain capture, so real content must be computed
  from an escape-preserving capture. Dim toggles: \\x1b[2m on; \\x1b[22m off; \\x1b[0m
  (or a bare \\x1b[m reset) off. Other SGR parameters do not affect dim.
  """
  out: list[str] = []
  dim = False
  i = 0
  while i < len(line):
    if line[i] == "\x1b":
      sgr = _SGR_RE.match(line, i)
      if sgr is not None:
        params = (sgr.group(1) or "0").split(";")
        j = 0
        while j < len(params):
          param = params[j]
          if param in ("", "0", "22"):
            dim = False
          elif param == "2":
            dim = True
          elif param in ("38", "48", "58"):
            # Extended color (38;5;n / 38;2;r;g;b): the arguments are color components,
            # not attributes -- live panes emit e.g. 48;5;22, whose 22 must not read as
            # dim-off (nor a 38;5;2 foreground as dim-on).
            j += 2 if j + 1 < len(params) and params[j + 1] == "5" else 4
          j += 1
        i = sgr.end()
        continue
      csi = _CSI_RE.match(line, i)
      i = csi.end() if csi is not None else i + 1
      continue
    if not dim:
      out.append(line[i])
    i += 1
  return "".join(out)


def first_nonempty_normalized_line(text: str) -> Optional[str]:
  for line in text.splitlines():
    normalized = " ".join(line.split())
    if normalized:
      return normalized
  return None


def _cursor_content(cursor_line: str) -> str:
  return cursor_line.partition("❯")[2].strip()


def _raw_pane_input_state(pane_text: str) -> PaneInputState:
  """Classify the active TUI cursor line from escape-preserving pane text."""
  cursor_line: Optional[str] = None
  for line in pane_text.splitlines():
    real = strip_dim_text(line)
    if "❯" in real:
      cursor_line = real
  if cursor_line is None:
    return PaneInputState(PaneInputKind.UNKNOWN)
  if _MENU_OPTION_RE.search(cursor_line) is not None:
    return PaneInputState(PaneInputKind.MENU, cursor_line.strip())
  return PaneInputState(PaneInputKind.PROMPT, _cursor_content(cursor_line))


@dataclass(frozen=True)
class PaneInputContext:
  """Context for interpreting ambiguous digit-leading Claude TUI cursor lines."""

  submitted_prompt: Optional[str] = None

  def menu_state_matches_submitted_prompt(self, state: PaneInputState) -> bool:
    submitted_first_line = (
        first_nonempty_normalized_line(self.submitted_prompt) if self.submitted_prompt is not None else None)
    if submitted_first_line is None:
      return False
    candidate = " ".join(_cursor_content(state.content).split())
    return candidate == submitted_first_line

  def classify(self, pane_text: str) -> PaneInputState:
    state = _raw_pane_input_state(pane_text)
    if state.kind == PaneInputKind.MENU and self.menu_state_matches_submitted_prompt(state):
      return PaneInputState(PaneInputKind.PROMPT, _cursor_content(state.content))
    return state

  def input_box_content(self, pane_text: str) -> Optional[str]:
    state = self.classify(pane_text)
    if state.kind == PaneInputKind.PROMPT:
      return state.content
    if state.kind == PaneInputKind.UNKNOWN:
      return None
    if state.kind == PaneInputKind.MENU:
      return None
    raise AssertionError(f"unhandled pane input kind: {state.kind!r}")

  def has_prompt(self, pane_text: str) -> bool:
    return self.classify(pane_text).kind == PaneInputKind.PROMPT

  def has_interactive_menu(self, pane_text: str) -> bool:
    state = _raw_pane_input_state(pane_text)
    if state.kind == PaneInputKind.MENU:
      return not self.menu_state_matches_submitted_prompt(state)
    if state.kind == PaneInputKind.PROMPT:
      return False
    if state.kind == PaneInputKind.UNKNOWN:
      return False
    raise AssertionError(f"unhandled pane input kind: {state.kind!r}")


def classify_pane_input(pane_text: str, submitted_prompt: Optional[str] = None) -> PaneInputState:
  """Classify the active TUI input control from an escape-preserving pane capture.

  The input box is the LAST ❯-prefixed line: after a submit the TUI echoes the
  submitted message in scrollback with the same ❯ prefix, so earlier ❯ lines must
  not count. Ghost suggestions and idle hints render dim and are stripped, so a box
  showing only ghost text reads as empty. A digit-leading real cursor line is a menu
  unless it matches the first non-empty line of `submitted_prompt`.
  """
  return PaneInputContext(submitted_prompt).classify(pane_text)


def input_box_content(pane_text: str, submitted_prompt: Optional[str] = None) -> Optional[str]:
  return PaneInputContext(submitted_prompt).input_box_content(pane_text)


def pane_has_prompt(pane_text: str, submitted_prompt: Optional[str] = None) -> bool:
  return PaneInputContext(submitted_prompt).has_prompt(pane_text)


def menu_state_matches_prompt(state: PaneInputState, submitted_prompt: Optional[str]) -> bool:
  return PaneInputContext(submitted_prompt).menu_state_matches_submitted_prompt(state)


def pane_has_interactive_menu(pane_text: str, submitted_prompt: Optional[str] = None) -> bool:
  return PaneInputContext(submitted_prompt).has_interactive_menu(pane_text)
