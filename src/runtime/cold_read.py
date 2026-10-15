"""The cold-read run's shared contract: prompt texts, the probe input, and the report body.

``prompts/cold_read.md`` is the only home of the cold-reader instructions and the seven
questions, and this module is their only reader: the artifact check CLI (a foreground cold
read) and the task execution layer (a cold-read child session's runs) both build from here,
so the foreground output and the child session's completion report carry identical lines.

A cold read asks one model, sight unseen beyond the page, seven fixed questions about one
HTML page. The report body states the page path and the sha256 of the page bytes the cold
read read, then one line per failed model attempt, then the answering model and the seven
answers verbatim — or, when every model failed, the ``every model failed (<n> tried)``
trailer.
"""

from __future__ import annotations

import pathlib

from src.runtime.spawner_prompt import load_marker_sections

PROMPT_FILENAME = "cold_read.md"
_SYSTEM_SECTION = "system_prompt"
_QUESTIONS_SECTION = "questions"
TRIGGER_PLACEHOLDER = "<trigger message verbatim>"

# The run-directory file one cold-read run writes at launch: the page path and
# the sha256 of the page bytes it read, plus the error its failure judgment
# recorded. The completion report reads every attempt back from these files.
EVIDENCE_FILENAME = "cold_read.json"


def load_texts(repo: pathlib.Path) -> tuple[str, str]:
  """The (system prompt, seven questions) pair, read fresh from ``prompts/cold_read.md``.

  The marker section keeps the file's own trailing newline out of both texts,
  so the probe input stays byte-identical to the pre-file constants.
  """
  sections = load_marker_sections(
      repo / "prompts" / PROMPT_FILENAME, (_SYSTEM_SECTION, _QUESTIONS_SECTION), extraction="cold-read")
  return sections[_SYSTEM_SECTION].rstrip("\n"), sections[_QUESTIONS_SECTION].rstrip("\n")


def probe_input(page_text: str, questions: str, trigger: str) -> str:
  """The cold-read run's whole input: the page text, one blank line, then the questions with
  the trigger filled in — the same string the CLI's foreground probe sends."""
  return f"{page_text}\n\n{questions.replace(TRIGGER_PLACEHOLDER, trigger)}"


def format_report(
    page_path: str,
    page_sha256: str,
    attempts: list[tuple[str, str]],
    backend_id: str | None,
    answer: str | None,
) -> str:
  """The cold-read report body: page and sha256 lines, one ``attempt`` line per failed run
  in order, then ``backend <id>`` plus the verbatim answers — or the all-failed trailer."""
  lines = [f"cold read of {page_path}", f"page sha256 {page_sha256}"]
  for backend, error in attempts:
    lines.append(f"attempt {backend} failed: {error}")
  if backend_id is None:
    lines.append(f"every model failed ({len(attempts)} tried)")
    return "\n".join(lines)
  assert answer is not None, "a successful cold-read report carries the answering backend's answer"
  lines.append(f"backend {backend_id}")
  lines.append(answer)
  return "\n".join(lines)
