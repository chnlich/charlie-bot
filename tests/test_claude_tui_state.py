from src.cli import claude_tui_state as tui_state


# Real `tmux capture-pane -e` fixtures from claude v2.1.174. Ghost suggestions and
# idle hints render SGR dim (\x1b[2m) and must read as EMPTY real content; pasted
# text (placeholder or literal) renders non-dim and must read as NON-empty.
_FIXTURE_IDLE_HINT = '\x1b[39m❯ \x1b[2mTry "how do I log an error?"\x1b[0m'
_FIXTURE_GHOST_SUGGESTION = "\x1b[39m❯ \x1b[2m3D latent draft suggestion\x1b[0m"
_FIXTURE_PASTE_PLACEHOLDER = "\x1b[39m❯ [Pasted text #1 +5 lines]"
_FIXTURE_LITERAL_PASTE = "\x1b[39m❯ short literal paste"
_FIXTURE_DIGIT_LEADING_LITERAL_PASTE = "\x1b[39m❯ 1/2/3/4/5 make a plan"
_FIXTURE_STARTUP_MENU = """\
\x1b[?25l╭────────────────────────────────────────────╮
│ Claude Code can help with this repository.   │
│ ❯ 1. Yes, try it                             │
│   2. No, keep using the terminal             │
╰────────────────────────────────────────────╯"""


def test_pane_input_kind_reserves_zero_for_unknown() -> None:
  assert tui_state.PaneInputKind.UNKNOWN == 0


def test_prompt_ready_state_detects_numbered_selection() -> None:
  plan_menu = (
      "Would you like to proceed?\n"
      "❯ 1. Yes, and auto-accept edits\n"
      "  2. Yes, and manually approve edits\n"
      "  3. No, keep planning\n")
  permission_menu = "│ ❯ 1. Yes                                  │"

  assert tui_state.prompt_ready_state(plan_menu).kind == tui_state.PaneInputKind.MENU
  assert tui_state.prompt_ready_state(permission_menu).kind == tui_state.PaneInputKind.MENU


def test_prompt_ready_state_detects_fullscreen_startup_menu() -> None:
  state = tui_state.prompt_ready_state(_FIXTURE_STARTUP_MENU)

  assert state.kind == tui_state.PaneInputKind.MENU
  assert "Yes, try it" in state.content


def test_prompt_ready_state_detects_empty_prompt() -> None:
  state = tui_state.prompt_ready_state(_FIXTURE_IDLE_HINT)

  assert state == tui_state.PaneInputState(tui_state.PaneInputKind.PROMPT, "")


def test_prompt_ready_state_detects_multiline_paste_placeholder() -> None:
  state = tui_state.prompt_ready_state(_FIXTURE_PASTE_PLACEHOLDER)

  assert state == tui_state.PaneInputState(
      tui_state.PaneInputKind.PROMPT, "[Pasted text #1 +5 lines]")


def test_prompt_ready_state_uses_last_cursor_line_over_scrollback_echo() -> None:
  pane = _FIXTURE_STARTUP_MENU + "\n" + _FIXTURE_IDLE_HINT

  state = tui_state.prompt_ready_state(pane)

  assert state == tui_state.PaneInputState(tui_state.PaneInputKind.PROMPT, "")


def test_prompt_send_input_state_treats_expected_digit_leading_prompt_as_prompt() -> None:
  state = tui_state.prompt_send_input_state(
      _FIXTURE_DIGIT_LEADING_LITERAL_PASTE, "1/2/3/4/5 make a plan")

  assert state == tui_state.PaneInputState(
      tui_state.PaneInputKind.PROMPT, "1/2/3/4/5 make a plan")


def test_prompt_send_input_state_keeps_unexpected_digit_leading_line_as_menu() -> None:
  state = tui_state.prompt_send_input_state(_FIXTURE_STARTUP_MENU, "1/2/3/4/5 make a plan")

  assert state.kind == tui_state.PaneInputKind.MENU
  assert "Yes, try it" in state.content


def test_quiet_turn_blocking_menu_ignores_prompt_and_output() -> None:
  idle_prompt = "❯ Try \"edit a file\""
  working = "✶ Running... (esc to interrupt)\n❯ "
  numbered_text = "Plan:\n1. first step\n2. second step"
  numbered_prompt = "1. bf16 dtype-aware parity\n2. capture the exact failure"
  numbered_prompt_echo_only = "❯ 1. bf16 dtype-aware parity\n  2. capture the exact failure"
  numbered_prompt_echo = (
      "❯ 1. Inspect the current failure\n"
      "  2. Patch the narrowest fix\n"
      "✶ Running... (esc to interrupt)\n" + _FIXTURE_IDLE_HINT)

  assert not tui_state.quiet_turn_has_blocking_menu(idle_prompt, "")
  assert not tui_state.quiet_turn_has_blocking_menu(working, "")
  assert not tui_state.quiet_turn_has_blocking_menu(numbered_text, "")
  assert not tui_state.quiet_turn_has_blocking_menu(numbered_prompt_echo_only, numbered_prompt)
  assert not tui_state.quiet_turn_has_blocking_menu(numbered_prompt_echo, "")


def test_quiet_turn_blocking_menu_detects_current_menu_after_numbered_prompt_echo() -> None:
  pane = (
      "❯ 1. Inspect the current failure\n"
      "  2. Patch the narrowest fix\n"
      "Would you like to proceed?\n"
      "❯ 1. Yes, and auto-accept edits\n"
      "  2. No, keep planning\n")

  assert tui_state.quiet_turn_has_blocking_menu(
      pane, "1. Inspect the current failure\n2. Patch the narrowest fix")


def test_quiet_turn_blocking_menu_ignores_digit_leading_dim_ghost_suggestion() -> None:
  # A dim ghost suggestion starting with a digit must not read as a blocking menu:
  # in a plain capture this line is "❯ 3D latent ..." and would match the menu
  # pattern, aborting a healthy long turn. Dim-stripping leaves only the bare "❯ ".
  plain_equivalent = "❯ 3D latent draft suggestion"

  assert tui_state.quiet_turn_has_blocking_menu(plain_equivalent, "")  # plain capture is unsound
  assert not tui_state.quiet_turn_has_blocking_menu(_FIXTURE_GHOST_SUGGESTION, "")


def test_strip_dim_text_ignores_extended_color_arguments() -> None:
  # Live panes emit extended-color SGR (e.g. \x1b[48;5;22m diff backgrounds): the color
  # arguments are not attributes, so the 22 in 48;5;22 must not read as SGR 22 (dim off),
  # nor the 2 in a 38;5;2 foreground as SGR 2 (dim on).
  assert tui_state.strip_dim_text("\x1b[38;5;2m❯ real text\x1b[0m") == "❯ real text"
  assert tui_state.strip_dim_text("\x1b[2mghost\x1b[48;5;22m still ghost\x1b[0m real") == " real"


def test_prompt_ready_state_reads_dim_hint_and_ghost_as_empty() -> None:
  assert tui_state.prompt_ready_state(_FIXTURE_IDLE_HINT).content == ""
  assert tui_state.prompt_ready_state(_FIXTURE_GHOST_SUGGESTION).content == ""


def test_prompt_ready_state_reads_pasted_text_as_nonempty() -> None:
  assert tui_state.prompt_ready_state(_FIXTURE_PASTE_PLACEHOLDER).content == "[Pasted text #1 +5 lines]"
  assert tui_state.prompt_ready_state(_FIXTURE_LITERAL_PASTE).content == "short literal paste"


def test_prompt_ready_state_uses_last_prompt_line_so_scrollback_echo_reads_submitted() -> None:
  pane = ("❯ Fix the recurring prompt-submit race in src/cli/claude_sub.py. Details\n"
          "  follow on later lines.\n"
          "✶ Running... (esc to interrupt)\n" + _FIXTURE_IDLE_HINT + "\n")

  assert tui_state.prompt_ready_state(pane).content == ""


def test_prompt_ready_state_returns_unknown_without_prompt_line() -> None:
  state = tui_state.prompt_ready_state("✶ Running... (esc to interrupt)\n")

  assert state.kind == tui_state.PaneInputKind.UNKNOWN


def test_prompt_ready_state_detects_tall_composer_input_box() -> None:
  pane = """─────────────────────────────────────────
❯ [Artifact comments - /files/.../plan_comment_fix.html] (4)
  1. > 2 Fix - web/static/js/artifact-comments.js > "2
     Fix -
  2. > 2 Fix - ... > "Use cleanText(block, 400) ..."
     -> B
  3. > 2 Fix - ... > "buildBatchMessage numbers the first line ..."
     -> C
  4. > In scope > "Robust multi-line handling in the message builder."
     -> D
─────────────────────────────────────────
  bypass permissions on (shift+tab to cycle)
"""

  assert tui_state.prompt_ready_state(pane).kind == tui_state.PaneInputKind.PROMPT


def test_prompt_ready_state_returns_unknown_without_input_box() -> None:
  state = tui_state.prompt_ready_state("✶ Booting Claude...\n  Loading session\n")

  assert state.kind == tui_state.PaneInputKind.UNKNOWN
