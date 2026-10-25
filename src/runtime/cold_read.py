"""The cold-read contract: prompt texts, probe inputs, backend rotation and report body.

``prompts/cold_read.md`` is the only home of the cold-reader instructions and the seven
questions, and this module is their only reader: the artifact check CLI (a foreground cold
read) and the task execution layer (a cold-read child session's runs) both build from here,
so the foreground output and the child session's completion report carry identical lines.

A page cold read asks seven fixed questions about one HTML page. An ask cold read asks five fixed questions about one
Waiting on you item and its linked page when this host serves it. Both use the same light-backend rotation.
"""

from __future__ import annotations

import dataclasses
import json
import pathlib
from typing import TYPE_CHECKING

from src.runtime import spawner_prompt

if TYPE_CHECKING:
  from src.infra import config

PROMPT_FILENAME = "cold_read.md"
_SYSTEM_SECTION = "system_prompt"
_QUESTIONS_SECTION = "questions"
_ASK_SYSTEM_SECTION = "ask_system_prompt"
_ASK_QUESTIONS_SECTION = "ask_questions"
TRIGGER_PLACEHOLDER = "<trigger message verbatim>"

# The run-directory file one cold-read run writes at launch: the page path and
# the sha256 of the page bytes it read, plus the error its failure judgment
# recorded. The completion report reads every attempt back from these files.
EVIDENCE_FILENAME = "cold_read.json"


def load_texts(repo: pathlib.Path) -> tuple[str, str]:
  """The (system prompt, eight questions) pair, read fresh from ``prompts/cold_read.md``.

  The marker section keeps the file's own trailing newline out of both texts,
  so the probe input stays byte-identical to the pre-file constants.
  """
  sections = spawner_prompt.load_marker_sections(
      repo / "prompts" / PROMPT_FILENAME, (_SYSTEM_SECTION, _QUESTIONS_SECTION), extraction="cold-read")
  return sections[_SYSTEM_SECTION].rstrip("\n"), sections[_QUESTIONS_SECTION].rstrip("\n")


def load_ask_texts(repo: pathlib.Path) -> tuple[str, str]:
  """The (system prompt, five questions) pair for one Waiting on you item."""
  sections = spawner_prompt.load_marker_sections(
      repo / "prompts" / PROMPT_FILENAME, (_ASK_SYSTEM_SECTION, _ASK_QUESTIONS_SECTION), extraction="cold-read")
  return sections[_ASK_SYSTEM_SECTION].rstrip("\n"), sections[_ASK_QUESTIONS_SECTION].rstrip("\n")


def probe_input(page_text: str, questions: str, trigger: str) -> str:
  """The cold-read run's whole input: the page text, one blank line, then the questions with
  the trigger filled in — the same string the CLI's foreground probe sends."""
  return f"{page_text}\n\n{questions.replace(TRIGGER_PLACEHOLDER, trigger)}"


def ask_probe_input(item: dict, linked_page: str | None, questions: str) -> str:
  """The item JSON, optional host-served page text, and the ask questions in that order."""
  pieces = [json.dumps(item, ensure_ascii=False, indent=2)]
  if linked_page is not None:
    pieces.append(linked_page)
  pieces.append(questions)
  return "\n\n".join(pieces)


@dataclasses.dataclass
class LightProbeResult:
  """Failed backend attempts followed by the successful backend and its verbatim answer, when one answers."""

  attempts: list[tuple[str, str]]
  backend_id: str | None
  answer: str | None


def run_light_probe(cfg: config.CharlieBotConfig, prompt: str, system_prompt: str) -> LightProbeResult:
  """Send *prompt* to each available light backend in pool rotation order, with no request timeout."""
  import asyncio

  from src.runtime import backend_pool  # deferred: charliebot artifact check and charliebot web status set
  from src.runtime.hooks import backend_types  # deferred: charliebot artifact check and charliebot web status set

  if not cfg.backends.pool:
    raise ValueError("no light backends resolvable: backends.pool is empty")
  attempts: list[tuple[str, str]] = []
  for option in backend_pool.iter_members(cfg, caller="light_oneshot"):
    try:
      answer = asyncio.run(backend_types.build_backend(option, cfg).one_shot_text(prompt, system_prompt, timeout=None))
      return LightProbeResult(attempts=attempts, backend_id=option.id, answer=answer)
    except Exception as exc:
      attempts.append((option.id, f"{type(exc).__name__}: {exc}"))
      if backend_pool.is_quota_error(str(exc)):
        backend_pool.cool(option.id, None)
  return LightProbeResult(attempts=attempts, backend_id=None, answer=None)


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
