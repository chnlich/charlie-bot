"""The model exchange contract: prompts, request builders, and response parsing.

Both stages answer one content-only request each — the request carries every
byte of evidence inline and the response is one JSON object. No tools are
offered at any point, so the model has no read path beyond the supplied
evidence and no write path at all; isolation is a transport property, not a
prompt instruction.

The reviewer reads the same evidence and selection the editor read, plus the
editor's proposed complete entries. The editor's justifications (its ``reason``
fields) are withheld from the reviewer request and kept only in the run record;
evaluation answers have no channel into either request because the manifest
cannot carry them.
"""

import json
from dataclasses import dataclass, field

from pydantic import BaseModel, ConfigDict, ValidationError

from src.core.memory_replay.errors import ReplayModelOutputError
from src.core.memory_replay.manifest import Manifest, Theme
from src.core.memory_replay.retrieval import FeedbackSelection

EDITOR_PROMPT_VERSION = "memory-replay-editor-v1"
REVIEWER_PROMPT_VERSION = "memory-replay-reviewer-v1"

# The response contract, stated twice: prose for the model, types for the parser.
_RESPONSE_SHAPE = """{
  "entries": [
    {"action": "new" | "rewrite" | "delete" | "keep",
     "path": "entries/<topic>/<slug>.md",
     "text": "<complete entry file text, front matter included>",
     "source_refs": ["<ref>"],
     "reason": "<one sentence>"}
  ],
  "candidates": [
    {"source_ref": "<ref>", "outcome": "propose" | "no_change" | "needs_decision",
     "paths": ["entries/<topic>/<slug>.md"], "reason": "<one sentence>"}
  ]
}"""

EDITOR_SYSTEM = f"""You are the editor stage of an offline memory-curation replay. You read frozen
evidence and return complete proposed memory entries as one JSON object. The request is
content-only: there are no tools, nothing you receive is writable, and your reply must be
exactly one JSON object with no other text.

Decide:
- which existing entries to rewrite (complete replacement text), delete, or keep;
- which new complete entries to add;
- for every candidate under "## Candidate material", exactly one disposition row with outcome
  "propose" (you changed or added at least one path because of it), "no_change", or
  "needs_decision".

Constraints:
- The guideline below is the admission bar. Keep the mechanism a future action needs; drop
  details the owning documents already carry and instance specifics that do not change future
  actions. Prefer merging into the existing entry whose theme covers the candidate.
- A candidate marked "explicit remember request" still gets a visible row; if you do not act on
  it, its row's "reason" names the request and why nothing changed.
- An entry change you initiate that no candidate asked for still gets a "candidates" row whose
  "source_ref" is that existing entry's path and whose "paths" list it.
- Cite only source refs you were actually given, in the "source_refs" of the entries rows those
  refs support. Never invent refs, paths, or topics outside the given vocabulary.
- Paths are exactly "entries/<topic>/<slug>.md". "new" paths must not exist yet; "rewrite",
  "delete", and "keep" paths must be listed under "## Current entries".
- A "rewrite" or "new" "text" is the complete entry file (front matter, then body) and must
  differ from the current text. "delete" and "keep" rows carry no "text".
- "propose" rows list the paths changed for that candidate; "no_change" and "needs_decision"
  rows have empty "paths".

JSON shape:
{_RESPONSE_SHAPE}"""

REVIEWER_SYSTEM = f"""You are the reviewer stage of an offline memory-curation replay. You re-decide every
disposition yourself, with "no change" as the default, and you own the final content. You see
the same frozen evidence the editor saw, the same selected user edit examples, and the editor's
proposed complete entries; the editor's justifications are deliberately withheld, so judge the
proposed text on the evidence alone. The request is content-only: there are no tools, nothing
you receive is writable, and your reply must be exactly one JSON object with no other text.

Decide:
- for every candidate under "## Candidate material", exactly one final row with outcome
  "propose", "no_change", or "needs_decision";
- for every entry proposed under "## Editor proposals": keep it as proposed, delete it, or
  rewrite it (return the complete replacement text);
- entries you change that the editor did not propose get their own "entries" row, and a
  "candidates" row whose "source_ref" is that existing entry's path.

Constraints:
- The guideline below is the admission bar. New or rewritten facts need a source ref you were
  actually given; never invent refs, paths, or topics outside the given vocabulary.
- Paths are exactly "entries/<topic>/<slug>.md". A "rewrite" or "new" "text" is the complete
  entry file (front matter, then body) and must differ from the current text.
- "propose" rows list the paths changed for that candidate; "no_change" and "needs_decision"
  rows have empty "paths".

JSON shape:
{_RESPONSE_SHAPE}"""


class EntryOpSpec(BaseModel):
  """One model-returned entry operation (response contract, parsed form)."""

  model_config = ConfigDict(extra="forbid")

  action: str
  path: str
  text: str | None = None
  source_refs: list[str] = []
  reason: str


class CandidateRowSpec(BaseModel):
  """One model-returned candidate disposition (response contract, parsed form)."""

  model_config = ConfigDict(extra="forbid")

  source_ref: str
  outcome: str
  paths: list[str] = []
  reason: str


class ModelOutputSpec(BaseModel):
  model_config = ConfigDict(extra="forbid")

  entries: list[EntryOpSpec]
  candidates: list[CandidateRowSpec]


@dataclass
class EntryOp:
  action: str
  path: str
  text: str | None
  source_refs: list[str]
  reason: str


@dataclass
class CandidateRow:
  source_ref: str
  outcome: str
  paths: list[str]
  reason: str


@dataclass
class ThemeOutput:
  """One stage's parsed response for one theme: entry operations plus candidate dispositions."""

  entries: list[EntryOp]
  candidates: list[CandidateRow]
  raw: str = field(repr=False, default="")


def build_editor_request(manifest: Manifest, theme: Theme, selections: list[FeedbackSelection]) -> str:
  """The editor's user content: every byte of theme evidence, deterministically ordered."""
  parts = [f"# Memory curation replay — editor\n\nTheme: {theme.name}"]
  parts.append("## Guideline (admission policy)")
  parts.extend(_render_source(s) for s in manifest.guidelines())
  parts.append("## Current entries")
  entries = manifest.theme_sources(theme, "entry")
  parts.extend(f"### {s.path}\n{s.text.rstrip()}" for s in entries)
  parts.append("## Owning documents")
  parts.extend(_render_source(s) for s in manifest.theme_sources(theme, "document"))
  parts.append("## Candidate material")
  parts.extend(_render_candidate(s) for s in manifest.theme_sources(theme, "candidate"))
  parts.append(_render_feedback(selections))
  return "\n\n".join(parts) + "\n"


def build_reviewer_request(
    manifest: Manifest,
    theme: Theme,
    selections: list[FeedbackSelection],
    editor_output: ThemeOutput,
) -> str:
  """The reviewer's user content: the editor's evidence plus its proposals, without its reasons."""
  parts = [f"# Memory curation replay — reviewer\n\nTheme: {theme.name}"]
  parts.append("## Guideline (admission policy)")
  parts.extend(_render_source(s) for s in manifest.guidelines())
  parts.append("## Current entries")
  entries = manifest.theme_sources(theme, "entry")
  parts.extend(f"### {s.path}\n{s.text.rstrip()}" for s in entries)
  parts.append("## Owning documents")
  parts.extend(_render_source(s) for s in manifest.theme_sources(theme, "document"))
  parts.append("## Candidate material")
  parts.extend(_render_candidate(s) for s in manifest.theme_sources(theme, "candidate"))
  parts.append(_render_feedback(selections))
  parts.append(_render_editor_proposals(editor_output))
  return "\n\n".join(parts) + "\n"


def _render_source(source) -> str:
  return f"[ref: {source.ref}]\n{source.text.rstrip()}"


def _render_candidate(source) -> str:
  marker = " (explicit remember request)" if source.remember_request else ""
  return f"[ref: {source.ref}]{marker}\n{source.text.rstrip()}"


def _render_feedback(selections: list[FeedbackSelection]) -> str:
  lines = ["## Prior user feedback (selected)"]
  if not selections:
    lines.append("### (none selected: no prior comment matched this theme's principles or terms)")
    return "\n".join(lines)
  for selection in selections:
    example = selection.example
    head = f"### comment_event: {example.comment_event} (score {selection.score})"
    if selection.matched_principles:
      head += f" [matched principles: {', '.join(selection.matched_principles)}]"
    lines.append(head)
    lines.append(f"comment:\n{example.comment_text.rstrip()}")
    if example.approved_change is not None:
      lines.append(f"approved change (ref: {example.approved_change.approved_change_ref}):")
      lines.append(f"--- before ---\n{example.approved_change.before.rstrip()}")
      lines.append(f"--- after ---\n{example.approved_change.after.rstrip()}")
  return "\n".join(lines)


def _render_editor_proposals(editor_output: ThemeOutput) -> str:
  lines = ["## Editor proposals (complete proposed entries; editor justifications withheld)"]
  if not editor_output.entries:
    lines.append("### (no entry changes proposed)")
  for op in editor_output.entries:
    lines.append(f"### action: {op.action} — {op.path}")
    if op.text is not None:
      lines.append(op.text.rstrip())
  return "\n".join(lines)


def parse_model_output(raw: str, *, role: str) -> ThemeOutput:
  """Parse one stage's response into a :class:`ThemeOutput`; raise on any shape violation."""
  payload = parse_model_json(raw, role=role)
  try:
    spec = ModelOutputSpec.model_validate(payload)
  except ValidationError as e:
    raise ReplayModelOutputError(f"{role}: response does not match the required JSON shape: {e}") from e
  entries = [
      EntryOp(
          action=item.action, path=item.path, text=item.text, source_refs=list(item.source_refs), reason=item.reason)
      for item in spec.entries
  ]
  candidates = [
      CandidateRow(source_ref=row.source_ref, outcome=row.outcome, paths=list(row.paths), reason=row.reason)
      for row in spec.candidates
  ]
  return ThemeOutput(entries=entries, candidates=candidates, raw=raw)


def parse_model_json(raw: str, *, role: str) -> dict:
  """Extract the one JSON object from a model response, tolerating a markdown code fence."""
  text = raw.strip()
  if text.startswith("```"):
    lines = text.split("\n")
    if lines[0].startswith("```"):
      lines = lines[1:]
    if lines and lines[-1].strip() == "```":
      lines = lines[:-1]
    text = "\n".join(lines).strip()
  try:
    payload = json.loads(text)
  except json.JSONDecodeError:
    start, end = text.find("{"), text.rfind("}")
    if start < 0 or end <= start:
      raise ReplayModelOutputError(f"{role}: response carries no JSON object") from None
    try:
      payload = json.loads(text[start:end + 1])
    except json.JSONDecodeError as e:
      raise ReplayModelOutputError(f"{role}: response is not valid JSON: {e}") from e
  if not isinstance(payload, dict):
    raise ReplayModelOutputError(f"{role}: response JSON is not an object")
  return payload
