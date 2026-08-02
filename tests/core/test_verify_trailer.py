from __future__ import annotations

from src.core.verify_trailer import verify_result_trailer_error


EMPTY_ERROR = "Verifier final report is empty; expected a final `RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \\([0-9]+ approval\\))` line."
MALFORMED_ERROR = (
    "Verifier final report has a missing or malformed `RESULT:` trailer; expected a final "
    '`RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \\([0-9]+ approval\\))` line.'
)


def test_bare_clean_last_line_is_valid() -> None:
  assert verify_result_trailer_error("We checked everything.\n\nRESULT: clean") == ""


def test_bare_mismatch_last_line_is_valid() -> None:
  assert verify_result_trailer_error("RESULT: 2 mismatches (1 approval)") == ""


def test_bold_wrapped_last_line_is_valid() -> None:
  assert verify_result_trailer_error("**RESULT: 1 mismatch (0 approval)**") == ""


def test_backtick_wrapped_last_line_is_valid() -> None:
  assert verify_result_trailer_error("```\n`RESULT: clean`") == ""


def test_valid_line_followed_by_fence_is_valid() -> None:
  assert verify_result_trailer_error("RESULT: clean\n```") == ""


def test_valid_line_followed_by_prose_is_valid() -> None:
  report = "RESULT: clean\nNo further issues were found."
  assert verify_result_trailer_error(report) == ""


def test_first_valid_mention_from_end_is_used() -> None:
  report = "Related RESULT: 1 mismatch (0 approval)\n\nRESULT: clean"
  assert verify_result_trailer_error(report) == ""


def test_mismatch_without_approval_suffix_is_malformed() -> None:
  assert verify_result_trailer_error("RESULT: 1 mismatch") == MALFORMED_ERROR


def test_empty_report_is_empty_error() -> None:
  assert verify_result_trailer_error("") == EMPTY_ERROR


def test_whitespace_only_report_is_empty_error() -> None:
  assert verify_result_trailer_error("  \n\n  ") == EMPTY_ERROR


def test_no_result_anywhere_is_malformed() -> None:
  assert verify_result_trailer_error("everything looks good") == MALFORMED_ERROR