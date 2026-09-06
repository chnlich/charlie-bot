from __future__ import annotations

import pytest

from src.core.verify_trailer import verify_result_trailer_error

EMPTY_ERROR = "Verifier final report is empty; expected a final `RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \\([0-9]+ approval\\))` line."
MALFORMED_ERROR = (
    "Verifier final report has a missing or malformed `RESULT:` trailer; expected a final "
    '`RESULT: (?:clean|[1-9][0-9]* mismatch(?:es)? \\([0-9]+ approval\\))` line.')


@pytest.mark.parametrize(
    "report",
    [
        "We checked everything.\n\nRESULT: clean",  # prose then a bare trailer
        "RESULT: 2 mismatches (1 approval)",  # mismatch verdict, no prose
        "**RESULT: 1 mismatch (0 approval)**",  # bold-wrapped trailer
        "```\n`RESULT: clean`",  # backtick-wrapped trailer inside an unclosed fence
        "RESULT: clean\n```",  # trailer followed by a closing fence
        "RESULT: clean\nNo further issues were found.",  # trailer followed by prose
        "Related RESULT: 1 mismatch (0 approval)\n\nRESULT: clean",  # an earlier mention loses to the last valid line
    ])
def test_valid_report_passes(report: str) -> None:
  assert verify_result_trailer_error(report) == ""


@pytest.mark.parametrize(
    ("report", "error"),
    [
        ("RESULT: 1 mismatch", MALFORMED_ERROR),  # missing the approval suffix
        ("", EMPTY_ERROR),
        ("  \n\n  ", EMPTY_ERROR),  # whitespace-only report
        ("everything looks good", MALFORMED_ERROR),  # no RESULT line anywhere
    ])
def test_invalid_report_reports_the_error(report: str, error: str) -> None:
  assert verify_result_trailer_error(report) == error
