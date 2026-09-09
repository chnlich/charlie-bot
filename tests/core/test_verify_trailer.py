from __future__ import annotations

import json
from pathlib import Path

import pytest

from core.byte_count_open import install_byte_counting_open
from src.api.message_utils import extract_text_from_message
from src.core.verify_trailer import _resolve_final_report, verify_result_trailer_error

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


# ---------------------------------------------------------------------------
# read_verify_final_report's from-the-end resolve walk
# ---------------------------------------------------------------------------


def _reference_report(events_path: Path) -> str:
  """The whole-list walk the from-the-end resolve must reproduce verbatim."""
  events = [json.loads(line) for line in events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
  for ev in reversed(events):
    if ev.get("type") != "result":
      continue
    result = ev.get("result")
    if isinstance(result, str) and result.strip():
      return result
    break
  for ev in reversed(events):
    if ev.get("type") != "assistant":
      continue
    message = ev.get("message") if isinstance(ev.get("message"), dict) else None
    text = extract_text_from_message(message)
    if text.strip():
      return text
  return ""


def _assistant_event(text: str) -> dict:
  return {"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}


def _corpus_cases() -> dict[str, list[dict]]:
  assistant = _assistant_event
  return {
      "result_payload_at_tail": [assistant("checked"), {
          "type": "result",
          "result": "done\nRESULT: clean"
      }],
      "empty_payload_falls_back_newer_assistant":
          [
              assistant("older"),
              {
                  "type": "result",
                  "result": ""
              },
              assistant("newer"),
          ],
      "non_string_payload_falls_back": [assistant("the text"), {
          "type": "result",
          "result": {
              "code": 0
          }
      }],
      "no_result_event": [assistant("first"), {
          "type": "tool_result",
          "content": "x"
      }, assistant("second")],
      "assistant_after_result": [
          assistant("before"),
          {
              "type": "result",
              "result": "kept"
          },
          assistant("after"),
      ],
      "empty_assistant_between": [
          assistant(""),
          {
              "type": "result",
              "result": ""
          },
          assistant("  "),
          assistant("real"),
      ],
      "neither_shape": [{
          "type": "tool_result",
          "content": "x"
      }],
  }


@pytest.mark.parametrize("case", sorted(_corpus_cases()))
def test_resolve_final_report_matches_whole_list_walk(tmp_path: Path, case: str) -> None:
  events_path = tmp_path / "events.jsonl"
  events_path.write_text("".join(json.dumps(e) + "\n" for e in _corpus_cases()[case]), encoding="utf-8")
  assert _resolve_final_report(events_path) == _reference_report(events_path)


def test_resolve_final_report_missing_file_and_empty_log(tmp_path: Path) -> None:
  assert _resolve_final_report(tmp_path / "absent.jsonl") == ""
  events_path = tmp_path / "events.jsonl"
  events_path.write_text("", encoding="utf-8")
  assert _resolve_final_report(events_path) == ""


def test_resolve_final_report_reads_only_the_tail_window_on_the_fallback_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
  # The empty-payload fallback shape — the reviewer's finding: the newest
  # non-empty assistant sits behind the tail result event, so both judgments
  # settle inside one window and the walk must not read the older bytes.
  target = tmp_path / "events.jsonl"
  with target.open("wb") as f:
    for i in range(2000):  # ~14 KB per event: the early corpus spans several windows
      f.write(
          (json.dumps({
              "type": "assistant",
              "message": {
                  "content": [{
                      "type": "text",
                      "text": f"early {i}"
                  }]
              }
          }) + "\n").encode())
    f.write(
        (
            json.dumps(
                {
                    "type": "assistant",
                    "message": {
                        "content": [{
                            "type": "text",
                            "text": "the report\nRESULT: clean"
                        }]
                    }
                }) + "\n").encode())
    f.write((json.dumps({"type": "result", "result": ""}) + "\n").encode())

  read_bytes = install_byte_counting_open(monkeypatch)
  assert _resolve_final_report(target) == "the report\nRESULT: clean"
  assert 0 < sum(read_bytes) <= 2 * 512 * 1024  # the newest window plus the fallback's one older window
