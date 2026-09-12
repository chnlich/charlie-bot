"""The v2 exchange contract, kept only so recorded v2 runs stay interpretable.

Replay runs under the v3 contract (``src/core/memory_replay/exchange.py``); the
paired comparison dispatches on a run's recorded prompt version and uses the
builders and prompts here — verbatim as they were when v2 was live — to
reconstruct v2 requests byte-identically and to validate v2 responses with
their original meanings. Nothing in the live pipeline reads this module.

The one semantic difference that matters: under v2, ``keep`` and ``delete``
rows must not carry ``source_refs`` (see ``validate.validate_theme_output_v2``).
A v2 run whose editor cited evidence on a kept entry therefore stays a failed
arm even though v3 allows optional citations there.
"""

from src.core.memory_replay.exchange import ThemeOutput
from src.core.memory_replay.manifest import Manifest, Theme
from src.core.memory_replay.retrieval import FeedbackSelection

EDITOR_PROMPT_VERSION = "memory-replay-editor-v2"
REVIEWER_PROMPT_VERSION = "memory-replay-reviewer-v2"

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
  "source_ref" is that entry's ref from "## Current entries" (never its store path) and whose
  "paths" list the entry's path.
- Cite only source refs you were actually given, in the "source_refs" of the entries rows those
  refs support. Never invent refs, paths, or topics outside the given vocabulary.
- Paths are exactly "entries/<topic>/<slug>.md", and a "new" entry's topic must be one of
  "## Allowed topics". "new" paths must not exist yet; "rewrite", "delete", and "keep" paths
  must be listed under "## Current entries".
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
  "candidates" row whose "source_ref" is that entry's ref from "## Current entries" (never its
  store path).

Constraints:
- The guideline below is the admission bar. New or rewritten facts need a source ref you were
  actually given; never invent refs, paths, or topics outside the given vocabulary.
- Paths are exactly "entries/<topic>/<slug>.md". A "rewrite" or "new" "text" is the complete
  entry file (front matter, then body) and must differ from the current text.
- "propose" rows list the paths changed for that candidate; "no_change" and "needs_decision"
  rows have empty "paths".

JSON shape:
{_RESPONSE_SHAPE}"""


def _evidence_parts(manifest: Manifest, theme: Theme, selections: list[FeedbackSelection]) -> list[str]:
  parts = [_render_topics(manifest)]
  parts.append("## Guideline (admission policy)")
  parts.extend(_render_source(s) for s in manifest.guidelines())
  parts.append("## Current entries")
  entries = manifest.theme_sources(theme, "entry")
  parts.extend(f"### {s.path} (ref: {s.ref})\n{s.text.rstrip()}" for s in entries)
  parts.append("## Owning documents")
  parts.extend(_render_source(s) for s in manifest.theme_sources(theme, "document"))
  parts.append("## Candidate material")
  parts.extend(_render_candidate(s) for s in manifest.theme_sources(theme, "candidate"))
  parts.append(_render_feedback(selections))
  return parts


def build_editor_request(manifest: Manifest, theme: Theme, selections: list[FeedbackSelection]) -> str:
  """The v2 editor's user content: prose sections, evidence inline under headings."""
  parts = [f"# Memory curation replay — editor\n\nTheme: {theme.name}"]
  parts.extend(_evidence_parts(manifest, theme, selections))
  return "\n\n".join(parts) + "\n"


def build_reviewer_request(
    manifest: Manifest,
    theme: Theme,
    selections: list[FeedbackSelection],
    editor_output: ThemeOutput,
) -> str:
  """The v2 reviewer's user content: the editor's evidence plus its proposals, without its reasons."""
  parts = [f"# Memory curation replay — reviewer\n\nTheme: {theme.name}"]
  parts.extend(_evidence_parts(manifest, theme, selections))
  parts.append(_render_editor_proposals(editor_output))
  return "\n\n".join(parts) + "\n"


def _render_topics(manifest: Manifest) -> str:
  return "## Allowed topics (the only topics an entry may use)\n" + "\n".join(manifest.topics)


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
      lines.extend(_render_approved_change(example.approved_change))
  return "\n".join(lines)


# An empty side of an approved change is real feedback (an approved deletion has an empty
# after, an approved creation an empty before), so the v2 rendering named the fact instead of
# leaving a bare section header the model could read as lost content. Nonempty sides render
# exactly as they did when v2 was live.
_EMPTY_BEFORE_MARKER = "(empty: the approved revision created this text)"
_EMPTY_AFTER_MARKER = "(empty: the approved revision deleted this text)"


def _render_approved_change(change) -> list[str]:
  before = change.before.rstrip() if change.before.strip() else _EMPTY_BEFORE_MARKER
  after = change.after.rstrip() if change.after.strip() else _EMPTY_AFTER_MARKER
  return [
      f"approved change (ref: {change.approved_change_ref}):",
      f"--- before ---\n{before}",
      f"--- after ---\n{after}",
  ]


def _render_editor_proposals(editor_output: ThemeOutput) -> str:
  lines = ["## Editor proposals (complete proposed entries; editor justifications withheld)"]
  if not editor_output.entries:
    lines.append("### (no entry changes proposed)")
  for op in editor_output.entries:
    lines.append(f"### action: {op.action} — {op.path}")
    if op.text is not None:
      lines.append(op.text.rstrip())
  return "\n".join(lines)
