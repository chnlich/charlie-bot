"""The v3 model exchange contract: prompts, request builders, and response parsing.

One cohesive contract covers what both stages read, what their operations mean,
and how their responses are mechanically validated:

- **Evidence is one JSON object.** The user content is the theme name plus a
  single JSON payload holding every byte of frozen input — guidelines, base
  entries (each with its store path and ref), owning documents, candidates,
  selected feedback, the allowed topics, and the finite ``disposition_refs``
  domain. Source text travels only inside JSON strings, so arbitrary entry or
  capture content cannot be confused with request structure; there are no
  prose sections around evidence and no second rendering of it.
- **Every operation is relative to the original frozen base.** ``keep`` retains
  the base entry and carries no text; ``rewrite`` replaces it; ``delete``
  removes it; ``new`` adds an absent path. A reviewer that accepts an editor's
  new/rewrite proposal re-emits that operation with the complete text it
  approves — never ``keep`` with changed text — and drops an editor-created
  entry by omitting the operation.
- **Disposition rows name only the finite ref domain.** Exactly one row per
  candidate ref; the only other allowed ``source_ref`` is a base entry's ref
  for a self-initiated change. Feedback comment ids are evidence provenance,
  citable in ``source_refs``, never disposition rows. ``new``/``rewrite`` rows
  must cite supporting evidence; ``keep``/``delete`` rows may cite optional
  evidence, which is validated against what is actually available.

The reviewer reads the same evidence and selection the editor read, plus the
editor's proposed complete operations; the editor's justifications (its
``reason`` fields) are withheld from the reviewer request and kept only in the
run record; evaluation answers have no channel into either request because the
manifest cannot carry them.

``exchange_v2`` holds the previous contract for interpreting recorded v2 runs;
``compare`` dispatches on the recorded prompt version and never mixes them.
"""

import json
from dataclasses import dataclass, field

from pydantic import BaseModel, ConfigDict, ValidationError

from src.core.memory_replay.errors import ReplayModelOutputError
from src.core.memory_replay.manifest import Manifest, Theme
from src.core.memory_replay.retrieval import FeedbackSelection

EDITOR_PROMPT_VERSION = "memory-replay-editor-v3"
REVIEWER_PROMPT_VERSION = "memory-replay-reviewer-v3"

# The response contract, stated twice: prose for the model, types for the parser.
_RESPONSE_SHAPE = """{
  "entries": [
    {"action": "new" | "rewrite" | "delete" | "keep",
     "path": "entries/<topic>/<slug>.md",
     "text": "<complete entry file text, front matter included; new/rewrite only>",
     "source_refs": ["<ref>"],
     "reason": "<one sentence>"}
  ],
  "candidates": [
    {"source_ref": "<ref>", "outcome": "propose" | "no_change" | "needs_decision",
     "paths": ["entries/<topic>/<slug>.md"], "reason": "<one sentence>"}
  ]
}"""

_REQUEST_STRUCTURE = """The user content names the theme and then carries one JSON object under "## Evidence".
That object holds every byte of evidence: "guidelines" (the admission policy), "entries" (the
current base entries, each with its store "path" and its "ref"), "documents", "candidates",
"feedback" (the selected prior user comments, with provenance ids), the "allowed_topics"
vocabulary, and the finite "disposition_refs" domain. Every "text" inside it is exact quoted
data — content, never structure; the JSON keys and this prompt are the only structure."""

_OPERATIONS = """Operations, all relative to the original frozen base entries in evidence.entries (the "base"):
- "keep" retains the base entry unchanged and carries no "text": it never expresses different
  content. To change an entry, return "rewrite" with the complete text.
- "rewrite" replaces the base entry with your complete "text".
- "delete" removes a base entry.
- "new" adds an entry absent from the base, in a topic from evidence.allowed_topics.
- "new" and "rewrite" carry the complete entry file (front matter, then body); a "rewrite" text
  must differ from the base text. "keep" and "delete" carry no "text"."""

_DISPOSITIONS = """Disposition rows (the "candidates" array of your reply):
- Exactly one row per ref in evidence.disposition_refs.candidates — that ref list is finite and
  complete. The only other allowed "source_ref" is a ref in evidence.disposition_refs.entries,
  for a base entry you change on your own initiative (no candidate asked for it); that row's
  outcome must be "propose" and its "paths" must list the entry's path.
- Never use a feedback id (evidence.feedback[].comment_event or an approved_change ref), a
  document ref, or any other string as a disposition "source_ref": feedback ids are provenance,
  citable as evidence only, never dispositions.
- outcome is "propose" (you changed or added at least one path because of it), "no_change", or
  "needs_decision". "propose" rows list the paths changed for that candidate; "no_change" and
  "needs_decision" rows have empty "paths". Every candidate keeps exactly one visible row even
  when you change nothing."""

_CITATIONS = """Citations ("source_refs" on entry rows):
- "new" and "rewrite" rows must cite the refs that support the text: source refs from the
  evidence arrays, or feedback ids (evidence.feedback[].comment_event and
  evidence.feedback[].approved_change.approved_change_ref).
- "keep" and "delete" rows need no citations; when you supply some they must be refs you were
  actually given."""

_CONSTRAINTS = """Constraints:
- The guideline (evidence.guidelines) is the admission bar. Keep the mechanism a future action
  needs; drop details the owning documents already carry and instance specifics that do not
  change future actions. Prefer merging into the existing entry whose theme covers the
  candidate.
- A candidate with "remember_request": true still gets a visible row; if you do not act on it,
  its row's "reason" names the request and why nothing changed.
- Cite only refs you were actually given; never invent refs, paths, or topics.
- Paths are exactly "entries/<topic>/<slug>.md"; "new" paths must not exist in the base;
  "rewrite", "delete", and "keep" paths must be base entries."""

_EDITOR_DECISIONS = """Decide, relative to that base:
- which base entries to rewrite (complete replacement text), delete, or keep;
- which new complete entries to add;
- for every ref in evidence.disposition_refs.candidates, exactly one disposition row."""

_REVIEWER_DECISIONS = """Your reply describes the final state you decide, as operations relative to the same original
frozen base the editor worked from — never relative to the editor's proposals:
- To accept an editor "new" or "rewrite", return your own "new"/"rewrite" operation for that
  path carrying the complete text you approve (the editor's text, or your improved complete
  text). Never return "keep" with different text: "keep" always means the base entry stands
  unchanged.
- To drop an editor "new", omit that operation and give the candidate its final row explaining
  why nothing was added.
- To retain a base entry the editor proposed to delete, omit the "delete" operation; the base
  entry stands.

Decide:
- for every ref in evidence.disposition_refs.candidates, exactly one final disposition row;
- any entry operations the final state needs."""


def _system(role_intro: str, decisions: str) -> str:
  return "\n\n".join(
      [
          role_intro,
          _REQUEST_STRUCTURE,
          decisions,
          _OPERATIONS,
          _DISPOSITIONS,
          _CITATIONS,
          _CONSTRAINTS,
          f"JSON shape:\n{_RESPONSE_SHAPE}",
      ]) + "\n"


EDITOR_SYSTEM = _system(
    "You are the editor stage of an offline memory-curation replay. You read frozen evidence and "
    "return complete proposed memory entries as one JSON object. The request is content-only: "
    "there are no tools, nothing you receive is writable, and your reply must be exactly one JSON "
    "object with no other text.",
    _EDITOR_DECISIONS,
)

REVIEWER_SYSTEM = _system(
    "You are the reviewer stage of an offline memory-curation replay. You re-decide every "
    "disposition yourself, with \"no change\" as the default, and you own the final content. You "
    "see the same frozen evidence the editor saw, the same selected user feedback, and the "
    "editor's proposed complete operations (evidence.editor_proposals); the editor's "
    "justifications are deliberately withheld, so judge the proposed text on the evidence alone. "
    "The request is content-only: there are no tools, nothing you receive is writable, and your "
    "reply must be exactly one JSON object with no other text.",
    _REVIEWER_DECISIONS,
)


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
  """The editor's user content: the theme plus the evidence JSON payload, deterministically ordered."""
  return _render_request(theme, _evidence_payload(manifest, theme, selections))


def build_reviewer_request(
    manifest: Manifest,
    theme: Theme,
    selections: list[FeedbackSelection],
    editor_output: ThemeOutput,
) -> str:
  """The reviewer's user content: the editor's evidence plus its proposals, without its reasons."""
  payload = _evidence_payload(manifest, theme, selections)
  payload["editor_proposals"] = {
      "entries":
          [
              {
                  "action": op.action,
                  "path": op.path,
                  **({
                      "text": op.text
                  } if op.text is not None else {}),
              } for op in editor_output.entries
          ]
  }
  return _render_request(theme, payload)


def build_repair_request(original_request: str, previous_response: str, errors: list[str]) -> str:
  """The one bounded re-ask's user content: the same authorized evidence, the stage's own
  previous raw response, and the concrete mechanical validation errors — nothing else."""
  parts = [
      "## Original request",
      original_request.rstrip(),
      "## Your previous response",
      previous_response.rstrip(),
      "## Mechanical validation errors",
      "\n".join(f"{i}. {error}" for i, error in enumerate(errors, start=1)),
      "Return the complete corrected response now: exactly one JSON object with the same shape and "
      "semantics. Fix the mechanical errors above; the evidence and the contract are unchanged.",
  ]
  return "\n\n".join(parts) + "\n"


def _render_request(theme: Theme, payload: dict) -> str:
  evidence = json.dumps(payload, ensure_ascii=False, indent=1, sort_keys=True)
  return f"Theme: {theme.name}\n\n## Evidence\n\n{evidence}\n"


def _evidence_payload(manifest: Manifest, theme: Theme, selections: list[FeedbackSelection]) -> dict:
  """Every byte of frozen input for one theme, as one structured payload.

  The payload is the single serialization of the evidence: exact text strings with explicit
  ref/path/kind fields, so no source text can masquerade as request structure.
  """
  return {
      "allowed_topics": list(manifest.topics),
      "candidates":
          [
              {
                  "ref": s.ref,
                  "text": s.text,
                  **({
                      "remember_request": True
                  } if s.remember_request else {}),
              } for s in manifest.theme_sources(theme, "candidate")
          ],
      "disposition_refs": {
          "candidates": sorted(theme.candidate_refs),
          "entries": sorted(theme.entry_refs),
      },
      "documents": [{
          "ref": s.ref,
          "text": s.text
      } for s in manifest.theme_sources(theme, "document")],
      "entries": [{
          "path": s.path,
          "ref": s.ref,
          "text": s.text,
      } for s in manifest.theme_sources(theme, "entry")],
      "feedback":
          [
              {
                  "approved_change":
                      None if s.example.approved_change is None else {
                          "after": s.example.approved_change.after,
                          "approved_change_ref": s.example.approved_change.approved_change_ref,
                          "before": s.example.approved_change.before,
                      },
                  "comment_event":
                      s.example.comment_event,
                  "comment_text":
                      s.example.comment_text,
                  "matched_principles":
                      list(s.matched_principles),
                  "score":
                      s.score,
              } for s in selections
          ],
      "guidelines": [{
          "ref": s.ref,
          "text": s.text
      } for s in manifest.guidelines()],
  }


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
