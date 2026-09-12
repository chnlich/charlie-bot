"""The experimental variant contracts: one owning module for the fixed-input comparison.

The approved redesign's evaluation needs the *original* curation judgment flow and each
proposed intervention runnable independently over identical frozen inputs. This module owns
that small, fixed set of named variants end to end — their prompts, their request builders,
their response contracts and validators, and the three behavioral dimensions that separate
them from the baseline:

- **feedback view** — what the stages see of the user's prior comments: the ``raw-history``
  view (every manifest comment verbatim with its provenance id, the adaptation of the
  production selector's user-message digest) or the ``selected-structured`` view (the existing
  relevance selection with original comments and approved before/after texts, as the new
  design supplies it);
- **rationale visibility** — whether the reviewer request carries the selector's handoff (its
  disposition rows and the three Action/Home/Brevity proof lines) or withholds every such
  field, from the initial request through every repair request;
- **reviewer capability** — ``trim-only`` (the original reviewer: accept the selector's text,
  remove lines from it, reject new entries, restore the base; never write prose) or
  ``whole-entry`` (the redesigned reviewer: re-decide and rewrite complete entries).

Each single-intervention variant changes exactly one of these dimensions relative to
``baseline-original-flow``; the combined variant is the proposed design. Everything else —
the frozen source snapshot, the guideline, the allowed topics, the candidate set, the
content-only transport, the JSON evidence payload, the base-relative operation protocol, the
bounded mechanical recovery, and the validation machinery — is shared, so a variant
difference is attributable to its declared dimension alone.

The citation domain is derived here from what a stage actually saw (the theme's assigned
sources, the guideline, and exactly the feedback ids its view exposed), closing the
unassigned-document gap recorded in memory_replay_visible_evidence_finding.md for the
experimental contracts. Recorded v2/v3 runs keep their original wider meanings elsewhere.

The runner and the paired comparison both dispatch through :class:`ExperimentContract`; the
CLI only resolves names. Nothing in this module reads the live memory store, the scoring
answers, another variant's outputs, or any pilot expectation.
"""

from collections.abc import Callable
from dataclasses import dataclass

from pydantic import BaseModel, ConfigDict, ValidationError

from src.core.memory_replay import exchange
from src.core.memory_replay.errors import (
    ReplayError,
    ReplayModelOutputError,
)
from src.core.memory_replay.exchange import (
    _CITATIONS,
    _CONSTRAINTS,
    _DISPOSITIONS,
    _EDITOR_DECISIONS,
    _OPERATIONS,
    _REQUEST_STRUCTURE,
    _RESPONSE_SHAPE,
    _REVIEWER_DECISIONS,
    CandidateRow,
    EntryOp,
    EntryOpSpec,
    ThemeOutput,
)
from src.core.memory_replay.manifest import Manifest, Theme
from src.core.memory_replay.retrieval import FeedbackSelection
from src.core.memory_replay.validate import canonical_text, theme_output_errors

# The one user rule that overrides the frozen guideline's stale language clause in every
# variant alike; the prompts state it identically so language never confounds a comparison.
LANGUAGE_RULE = (
    "Language: entry prose is written in English (code, paths, identifiers, and commands stay "
    "English). Where the supplied guideline's language clause conflicts, the user's explicit "
    "English-memory requirement governs; this experiment follows it in every variant alike.")

# --- the three behavioral dimensions -------------------------------------------

RAW_HISTORY_VIEW = "raw-history"
SELECTED_STRUCTURED_VIEW = "selected-structured"

RATIONALE_VISIBLE = "visible"
RATIONALE_HIDDEN = "hidden"

TRIM_ONLY = "trim-only"
WHOLE_ENTRY = "whole-entry"


def _compose(*blocks: str) -> str:
  return "\n\n".join(blocks) + "\n"


def _shape() -> str:
  return f"JSON shape:\n{_RESPONSE_SHAPE}"


# The authoritative sources of the baseline adaptation, pinned and fingerprinted so every audit of
# the adapted original flow checks it against exactly these texts (read with `git show`).
BASELINE_SOURCE_REVISION = "183fb29fa91b03a2c457ff7c73846299a44c420f"
BASELINE_SOURCE_ANCHORS = (
    "Source anchors: this baseline adapts the original production curation prompts "
    "prompts/cron/memory_curator/memory_selector.md (sha256 "
    "bdefc53d138c73e03d2f5f61c3264ea7ddfd2b5b51ad2bac10c81a5726d8f59c) and "
    "prompts/cron/memory_curator/memory_reviewer.md (sha256 "
    "73a2c360667c6c4186f29bcd3e02a0a948cffcbcb5a4bf5bdfad558bff52c147) at git revision "
    f"{BASELINE_SOURCE_REVISION}.")

# --- the baseline selector: the original judgment flow, adapted to frozen inputs ---------------

_SELECTOR_ROLE = (
    "You are the selector stage of an offline memory-curation experiment: the original production "
    "memory-selector judgment flow, adapted to frozen inputs. You curate staged candidates one by one, "
    "merge-first, and hand off every disposition with its admission proof lines. You read frozen evidence "
    "and return complete proposed memory entries as one JSON object. The request is content-only: there "
    "are no tools, nothing you receive is writable, and your reply must be exactly one JSON object with "
    "no other text.")

_SELECTOR_ADAPTATION = (
    "What this adaptation preserves from the original selector, and what the frozen-input experiment "
    "replaces:\n"
    "- Curate candidate by candidate. The default action for a passing candidate is a merge into the "
    "existing entry whose theme covers it (the original revise): return it as a \"rewrite\" of that "
    "entry, so the final diff shows before and after.\n"
    "- Admit a new entry only when no existing entry's theme covers the candidate and the title honestly "
    "describes the whole content after the change (the original admit): return it as a \"new\" operation "
    "with the complete header (scope, topic, audience, title) and body.\n"
    "- Reject the rest the way the original handoff sheet listed them: a one-line reason naming the "
    "question the candidate could not answer or why it does not belong (wrong home, theme already "
    "covered, dishonest title), in the candidate's visible disposition row.\n"
    "- Every candidate you act on (outcome \"propose\") hands off its three admission proof lines in the "
    "row's \"proofs\", each answering its question in one sentence: \"action\" — when will this be used "
    "again, and what will it change; \"home\" — why is the store the cheapest home, answered by checking "
    "the other homes (owning docs, skills, LESSONS.md, run dirs, trackers) and naming the reader and the "
    "delivery path; \"brevity\" — what the trim removed, or the line count when the draft already sat at "
    "the bar. A question that finds no answer is the signal to reject the candidate instead.\n"
    "- The frozen-input experiment replaces the production steps that touched the live store: staged "
    "captures arrive as evidence.candidates, the admission guideline arrives as evidence.guidelines (the "
    "production prompt read the llm-context-guideline skill and the master prompt's Writing Style "
    "section; both are frozen into the supplied guideline here), the working-tree edits become "
    "complete-entry operations relative to the original frozen base, the lint pass runs mechanically "
    "after your reply, and the handoff sheet becomes the recorded dispositions and proofs.")

_FEEDBACK_RAW_HISTORY = (
    "User feedback: evidence.feedback_history is the raw history of the user's prior comments about this "
    "store, verbatim, each with its provenance id (\"comment_event\") — the same raw user-message digest "
    "the production selector reads. These comments are evidence of what the user has said, not rules, "
    "and this view carries no approved before/after revisions.")

_FEEDBACK_SELECTED = (
    "User feedback: evidence.feedback carries the prior user comments selected as relevant to this "
    "theme, each with its original comment text, its provenance id, and — when one exists — the approved "
    "before/after revision under \"approved_change\". An empty side is real feedback: an approved "
    "deletion has an empty \"after\", an approved creation an empty \"before\".")

_SELECTOR_PROOFS = (
    "Admission proofs on disposition rows: every row with outcome \"propose\" must carry \"proofs\": "
    "{\"action\", \"home\", \"brevity\"} — the three proof lines above, written by you for this "
    "candidate. Rows with outcome \"no_change\" or \"needs_decision\" carry no \"proofs\".")


def _selector_system(feedback_block: str) -> str:
  return _compose(
      _SELECTOR_ROLE,
      _REQUEST_STRUCTURE,
      _SELECTOR_ADAPTATION,
      feedback_block,
      _EDITOR_DECISIONS,
      _OPERATIONS,
      _DISPOSITIONS,
      _SELECTOR_PROOFS,
      _CITATIONS,
      _CONSTRAINTS,
      _shape(),
      LANGUAGE_RULE,
  )


# --- the baseline reviewer: the original gating flow, adapted to frozen inputs -----------------

_REVIEWER_ROLE = (
    "You are the reviewer stage of an offline memory-curation experiment: the original production "
    "memory-reviewer gating flow, adapted to frozen inputs. You re-decide every disposition yourself, "
    "with \"no change\" as the default, and you gate the selector's proposed changes line by line. You "
    "see the same frozen evidence the selector saw. The request is content-only: there are no tools, "
    "nothing you receive is writable, and your reply must be exactly one JSON object with no other text.")

_HANDOFF_VISIBLE = (
    "You also see the selector's handoff sheet — its disposition rows and their three admission proof "
    "lines — under evidence.editor_proposals.dispositions. Weigh it as the original reviewer weighed the "
    "handoff: it says what the selector did and which admission questions it answered; it does not "
    "exempt any line from your gate.")

_HANDOFF_HIDDEN = (
    "The selector's handoff sheet — its disposition rows and their three admission proof lines — is "
    "withheld from this request; judge the proposed text on the evidence alone, as the redesigned "
    "reviewer must.")

_TRIM_GATE = (
    "What this adaptation preserves from the original reviewer:\n"
    "- Gate every changed line of every proposed entry: ask whether it still holds after the model, the "
    "fix, and the next run are replaced, and check it against the guideline's entry form (narrative, "
    "length, phrasing). A line that fails is deleted or trimmed.\n"
    "- You write no new entry prose. Whatever text you return for a path must be the selector's proposed "
    "text for that path with whole lines removed — never new or recombined prose, and never text on a "
    "path the selector did not propose text for.\n"
    "- A file whose whole change fails is restored to the base; an admitted entry that wholly fails is "
    "rejected (its proposed new entry is dropped). Every reversal is visible in the affected "
    "candidate's final disposition row with a one-sentence reason, as the original reviewer's reject "
    "rows were.")

_TRIM_DECISIONS = (
    "Your reply describes the final state you decide, as operations relative to the same original frozen "
    "base the selector worked from — never relative to the selector's proposals:\n"
    "- To accept a selector \"new\"/\"rewrite\", return that same operation for the path with the text "
    "you approve: the selector's text, or that text with failing lines removed (every line of your text "
    "must be a verbatim line of the selector's proposed text for that path, in the selector's order).\n"
    "- To drop a selector \"new\", omit that operation and give the candidate its final row explaining "
    "the reversal.\n"
    "- To restore a base entry the selector proposed to change, omit the operation — the base entry "
    "stands (\"keep\" is the explicit form of the same restore and carries no text).\n"
    "- Return \"delete\" only to confirm a selector \"delete\" of the same path.\n"
    "- A change you initiate on a path with no selector text proposal is not available to you.")

_WHOLE_ENTRY_AUTHORITY = (
    "What this variant changes — whole-entry review, the redesigned reviewer's authority: you own the "
    "final content. You may rewrite a proposed entry as a whole with your own complete text (concision "
    "and reorganization supported by the evidence), delete or keep entries, and restore the base — "
    "everything the trim-only reviewer is barred from. New or rewritten facts still need a source ref "
    "you were actually given, and every reversal stays visible in the affected candidate's final row.")


def _reviewer_system(handoff_block: str, gate_block: str, decisions_block: str) -> str:
  return _compose(
      _REVIEWER_ROLE,
      _REQUEST_STRUCTURE,
      handoff_block,
      gate_block,
      decisions_block,
      _OPERATIONS,
      _DISPOSITIONS,
      _CITATIONS,
      _CONSTRAINTS,
      _shape(),
      LANGUAGE_RULE,
  )


SELECTOR_RAW_HISTORY_SYSTEM = _selector_system(_FEEDBACK_RAW_HISTORY)
SELECTOR_SELECTED_SYSTEM = _selector_system(_FEEDBACK_SELECTED)

TRIM_RAW_HISTORY_VISIBLE_SYSTEM = _reviewer_system(_HANDOFF_VISIBLE, _TRIM_GATE, _TRIM_DECISIONS)
TRIM_RAW_HISTORY_HIDDEN_SYSTEM = _reviewer_system(_HANDOFF_HIDDEN, _TRIM_GATE, _TRIM_DECISIONS)
TRIM_SELECTED_VISIBLE_SYSTEM = _reviewer_system(_HANDOFF_VISIBLE, _TRIM_GATE, _TRIM_DECISIONS)
WHOLE_ENTRY_RAW_HISTORY_VISIBLE_SYSTEM = _reviewer_system(_HANDOFF_VISIBLE, _WHOLE_ENTRY_AUTHORITY, _REVIEWER_DECISIONS)

COMBINED_EDITOR_SYSTEM = exchange.EDITOR_SYSTEM.rstrip() + "\n\n" + LANGUAGE_RULE + "\n"
COMBINED_REVIEWER_SYSTEM = exchange.REVIEWER_SYSTEM.rstrip() + "\n\n" + LANGUAGE_RULE + "\n"

# --- visible-evidence citation domains ---------------------------------------------------------


def visible_refs(
    manifest: Manifest,
    theme: Theme,
    selections: list[FeedbackSelection],
    feedback_view: str,
) -> set[str]:
  """The citation domain one stage actually saw, per the recorded evidence finding.

  The theme's assigned sources plus the guideline, and exactly the feedback ids its declared
  view exposed: every comment's provenance id under the raw-history view; only the selected
  comments — and their approved-change refs only when their before/after content was exposed —
  under the selected-structured view. A response may not cite evidence its request never
  carried, on the initial attempt, on a repair, or in a later comparison.
  """
  refs = {s.ref for s in manifest.theme_sources(theme, "candidate")}
  refs |= {s.ref for s in manifest.theme_sources(theme, "entry")}
  refs |= {s.ref for s in manifest.theme_sources(theme, "document")}
  refs |= {s.ref for s in manifest.guidelines()}
  if feedback_view == RAW_HISTORY_VIEW:
    refs |= {f.comment_event for f in manifest.feedback_examples}
  elif feedback_view == SELECTED_STRUCTURED_VIEW:
    for selection in selections:
      refs.add(selection.example.comment_event)
      if selection.example.approved_change is not None:
        refs.add(selection.example.approved_change.approved_change_ref)
  else:
    raise ReplayError(f"unknown feedback view: {feedback_view!r}")
  return refs


def feedback_history_payload(manifest: Manifest) -> list[dict]:
  """The raw-history feedback view: every pool comment verbatim with its provenance id.

  This is the fixed-input adaptation of the production selector's user-message digest. It
  carries comment texts and provenance only — no approved before/after revisions, which the
  production selector never saw and which the feedback intervention alone supplies.
  """
  return [{
      "comment_event": f.comment_event,
      "comment_text": f.comment_text,
  } for f in manifest.feedback_examples]


# --- request builders --------------------------------------------------------------------------


def build_selector_request(
    manifest: Manifest,
    theme: Theme,
    selections: list[FeedbackSelection],
    *,
    feedback_view: str,
) -> str:
  """The baseline selector's user content: the evidence context plus the variant's feedback view."""
  payload = exchange.evidence_context_payload(manifest, theme)
  if feedback_view == RAW_HISTORY_VIEW:
    payload["feedback_history"] = feedback_history_payload(manifest)
  else:
    payload["feedback"] = exchange.feedback_selections_payload(selections)
  return exchange.render_evidence_request(theme, payload)


def build_selector_reviewer_request(
    manifest: Manifest,
    theme: Theme,
    selections: list[FeedbackSelection],
    editor_output: ThemeOutput,
    *,
    feedback_view: str,
    rationale_visible: bool,
) -> str:
  """The baseline reviewer's user content: the selector's evidence plus its proposals.

  The handoff (disposition rows and their proof lines) rides in
  ``editor_proposals.dispositions`` exactly when the variant's rationale visibility says so;
  when hidden, every such field stays out of this request and of every repair request built
  from it.
  """
  payload = exchange.evidence_context_payload(manifest, theme)
  if feedback_view == RAW_HISTORY_VIEW:
    payload["feedback_history"] = feedback_history_payload(manifest)
  else:
    payload["feedback"] = exchange.feedback_selections_payload(selections)
  proposals = {
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
  if rationale_visible:
    proposals["dispositions"] = [
        {
            "source_ref": row.source_ref,
            "outcome": row.outcome,
            "paths": list(row.paths),
            "reason": row.reason,
            **({
                "proofs": dict(row.proofs)
            } if row.proofs is not None else {}),
        } for row in editor_output.candidates
    ]
  payload["editor_proposals"] = proposals
  return exchange.render_evidence_request(theme, payload)


# --- the selector's response contract: the three proofs are model output -----------------------

PROOF_KEYS = ("action", "home", "brevity")


class _StrictModel(BaseModel):
  model_config = ConfigDict(extra="forbid")


class _ProofSpec(_StrictModel):
  action: str
  home: str
  brevity: str


class _SelectorCandidateRowSpec(_StrictModel):
  source_ref: str
  outcome: str
  paths: list[str] = []
  reason: str
  proofs: _ProofSpec | None = None


class _SelectorOutputSpec(_StrictModel):
  entries: list[EntryOpSpec]
  candidates: list[_SelectorCandidateRowSpec]


def parse_selector_output(raw: str, *, role: str) -> ThemeOutput:
  """Parse one selector response: the v3 shape plus the per-candidate admission proofs."""
  payload = exchange.parse_model_json(raw, role=role)
  try:
    spec = _SelectorOutputSpec.model_validate(payload)
  except ValidationError as e:
    raise ReplayModelOutputError(f"{role}: response does not match the required JSON shape: {e}") from e
  entries = [
      EntryOp(
          action=item.action, path=item.path, text=item.text, source_refs=list(item.source_refs), reason=item.reason)
      for item in spec.entries
  ]
  candidates = [
      CandidateRow(
          source_ref=row.source_ref,
          outcome=row.outcome,
          paths=list(row.paths),
          reason=row.reason,
          proofs=None if row.proofs is None else {
              "action": row.proofs.action,
              "brevity": row.proofs.brevity,
              "home": row.proofs.home,
          }) for row in spec.candidates
  ]
  return ThemeOutput(entries=entries, candidates=candidates, raw=raw)


def _proof_errors(output: ThemeOutput, *, role: str) -> list[str]:
  """The handoff proof requirement, checked on the stage's own rows: propose rows carry all
  three non-empty proof lines; other rows carry none."""
  errors: list[str] = []
  for row in output.candidates:
    if row.outcome == "propose":
      if row.proofs is None:
        errors.append(
            f"{role}: propose row for {row.source_ref!r} must carry the three admission proofs "
            f"({', '.join(PROOF_KEYS)})")
        continue
      for key in PROOF_KEYS:
        if not (row.proofs.get(key) or "").strip():
          errors.append(f"{role}: propose row for {row.source_ref!r} has an empty {key!r} proof line")
    elif row.proofs is not None:
      errors.append(
          f"{role}: only propose rows carry admission proofs; the {row.outcome} row for "
          f"{row.source_ref!r} must not")
  return errors


# --- validators ---------------------------------------------------------------------------------


def _mechanical_errors(
    output: ThemeOutput,
    *,
    role: str,
    manifest: Manifest,
    theme: Theme,
    selections: list[FeedbackSelection],
    feedback_view: str,
) -> list[str]:
  return theme_output_errors(
      output,
      role=role,
      manifest=manifest,
      theme=theme,
      allow_no_write_citations=True,
      available_refs=visible_refs(manifest, theme, selections, feedback_view))


def _selector_editor_errors(feedback_view: str) -> Callable:

  def errors(
      output: ThemeOutput, *, role: str, manifest: Manifest, theme: Theme, selections: list[FeedbackSelection],
      editor_output: ThemeOutput | None) -> list[str]:
    errors = _mechanical_errors(
        output, role=role, manifest=manifest, theme=theme, selections=selections, feedback_view=feedback_view)
    errors.extend(_proof_errors(output, role=role))
    return errors

  return errors


def _narrowed_v3_errors(feedback_view: str) -> Callable:
  """The v3 mechanical checks under the visible-evidence citation domain."""

  def errors(
      output: ThemeOutput, *, role: str, manifest: Manifest, theme: Theme, selections: list[FeedbackSelection],
      editor_output: ThemeOutput | None) -> list[str]:
    return _mechanical_errors(
        output, role=role, manifest=manifest, theme=theme, selections=selections, feedback_view=feedback_view)

  return errors


def _trim_capability_errors(output: ThemeOutput, *, role: str, editor_output: ThemeOutput) -> list[str]:
  """The narrow trim-only capability, checked against the exact selector output the reviewer saw.

  Allowed: confirming a selector new/rewrite with its own text or a line-removed form of it;
  confirming a selector delete; restoring a base entry (omit, or the explicit keep); dropping a
  selector new by omission. Forbidden: any text not removable line-by-line from the selector's
  proposed text, any operation on a path the selector never proposed text for, and deleting an
  entry the selector did not delete.
  """
  editor_ops = {op.path: op for op in editor_output.entries}
  errors: list[str] = []
  for op in output.entries:
    if op.action == "keep":
      continue  # the explicit base-restore; the standard checks pin it to a base entry with no text
    editor_op = editor_ops.get(op.path)
    if editor_op is None:
      errors.append(
          f"{role}: trim-only review returned {op.action} on {op.path}, which the selector never proposed "
          "text for; the reviewer writes no prose of its own")
      continue
    if op.action == "delete":
      if editor_op.action != "delete":
        errors.append(
            f"{role}: trim-only review returned delete on {op.path}; it may only confirm a selector "
            "delete, never delete an entry the selector proposed to keep or change")
      continue
    if op.action not in ("rewrite", "new"):
      continue
    if op.action != editor_op.action:
      errors.append(
          f"{role}: trim-only review returned {op.action} on {op.path}; the selector proposed "
          f"{editor_op.action} there, and the reviewer may only confirm or trim that operation")
      continue
    if editor_op.text is None:
      errors.append(
          f"{role}: trim-only review returned {op.action} on {op.path} but the selector's "
          f"{editor_op.action} carries no text to trim")
      continue
    violation = _line_removal_violation(op.text or "", editor_op.text)
    if violation:
      errors.append(f"{role}: trim-only review of {op.path}: {violation}")
  return errors


def _line_removal_violation(returned_text: str, proposed_text: str) -> str | None:
  """The mechanical form of the original reviewer's 'deleted or trimmed' under a no-new-prose
  capability: every returned line must be a verbatim line of the selector's proposed text, in
  the selector's order, each selector line used at most once. Within-line rewriting is not
  available to the reviewer; a failing line is removed whole. Greedy left-to-right matching is
  complete for this subsequence check."""
  proposed = canonical_text(proposed_text).split("\n")
  returned = canonical_text(returned_text or "").split("\n")
  cursor = 0
  for index, line in enumerate(returned):
    while cursor < len(proposed) and proposed[cursor] != line:
      cursor += 1
    if cursor == len(proposed):
      return (
          f"line {index + 1} ({line!r}) is not a verbatim line of the selector's proposed text in "
          "order; the reviewer removes whole lines and writes no prose")
    cursor += 1
  return None


def _trim_reviewer_errors(feedback_view: str) -> Callable:

  def errors(
      output: ThemeOutput, *, role: str, manifest: Manifest, theme: Theme, selections: list[FeedbackSelection],
      editor_output: ThemeOutput) -> list[str]:
    errors = _mechanical_errors(
        output, role=role, manifest=manifest, theme=theme, selections=selections, feedback_view=feedback_view)
    errors.extend(_trim_capability_errors(output, role=role, editor_output=editor_output))
    return errors

  return errors


# --- proposal feedback refs ---------------------------------------------------------------------


def _feedback_refs_all(manifest: Manifest, selections: dict[str, list[FeedbackSelection]]) -> list[dict]:
  """Raw-history variants saw the whole comment pool, so the proposal names all of it.

  ``approved_change_ref`` stays null: the raw-history view exposes comment texts and provenance
  only, so the approved revisions were never evidence these stages could cite — the same rule
  the citation domain enforces.
  """
  del selections
  return [{"comment_event": f.comment_event, "approved_change_ref": None} for f in manifest.feedback_examples]


def _feedback_refs_selected(manifest: Manifest, selections: dict[str, list[FeedbackSelection]]) -> list[dict]:
  """Selected-view variants saw the relevance selection, so the proposal names exactly that."""
  by_event = {f.comment_event: f for f in manifest.feedback_examples}
  events = sorted({s.example.comment_event for selected in selections.values() for s in selected})
  return [
      {
          "comment_event":
              event,
          "approved_change_ref":
              by_event[event].approved_change.approved_change_ref if by_event[event].approved_change else None,
      } for event in events
  ]


# --- the contract object ------------------------------------------------------------------------

_IDENTITY_FIELDS = (
    "name",
    "version",
    "editor_stage",
    "reviewer_stage",
    "feedback_view",
    "rationale_visibility",
    "reviewer_capability",
)


@dataclass(frozen=True)
class ExperimentContract:
  """One variant's complete behavior-affecting contract.

  This is the single home the runner, the paired comparison, and the CLI dispatch through:
  the prompts and their versions, the request builders, the parser, the per-stage validators
  (which close over the variant's feedback view and rationale visibility), the citation
  domain, and the identity payload that makes runs under different variants non-reusable
  against each other.
  """

  name: str
  title: str
  version: int
  editor_stage: str
  reviewer_stage: str
  feedback_view: str
  rationale_visibility: str
  reviewer_capability: str
  editor_prompt_version: str
  reviewer_prompt_version: str
  editor_system: str
  reviewer_system: str
  build_editor_request: Callable
  build_reviewer_request: Callable
  parse_editor_output: Callable
  parse_reviewer_output: Callable
  editor_errors: Callable
  reviewer_errors: Callable
  feedback_refs: Callable
  changes_vs_baseline: tuple[str, ...]
  notes: tuple[str, ...]
  experiment: bool = True

  def identity_payload(self) -> dict:
    """The behavioral definition that joins the run identity: changing any field invalidates reuse."""
    return {field: getattr(self, field) for field in _IDENTITY_FIELDS}

  def record_payload(self) -> dict:
    """The run-record section: the identity payload plus the audited prose definition.

    Non-experiment contracts record nothing, so standalone v2/v3 run records keep their exact
    historical shape and are never misread as variant runs.
    """
    if not self.experiment:
      return {}
    return {
        "variant":
            {
                **self.identity_payload(),
                "title": self.title,
                "changes_vs_baseline": list(self.changes_vs_baseline),
                "notes": list(self.notes),
                "prompt_versions": {
                    "editor": self.editor_prompt_version,
                    "reviewer": self.reviewer_prompt_version,
                },
            }
    }


def identity_fields_from_record(record: dict) -> dict:
  """The variant identity a recorded run declares; the comparison re-verifies it against the registry."""
  variant = record.get("variant")
  if not isinstance(variant, dict):
    raise ReplayError(
        "the run record declares no variant; this comparison reads experimental variant runs, not "
        "standalone v2/v3 replays")
  missing = [field for field in _IDENTITY_FIELDS if field not in variant]
  if missing:
    raise ReplayError(
        f"the run record's variant section is missing {', '.join(missing)}; the record changed after the run")
  return {field: variant[field] for field in _IDENTITY_FIELDS}


def contract_for_record(record: dict) -> ExperimentContract:
  """The registry contract a recorded variant run ran under; mismatching records fail loudly."""
  fields = identity_fields_from_record(record)
  contract = VARIANTS.get(fields["name"])
  if contract is None:
    raise ReplayError(f"the run record names variant {fields['name']!r}, which this registry does not define")
  if contract.identity_payload() != fields:
    raise ReplayError(
        f"the recorded definition of variant {fields['name']!r} no longer matches this registry's contract; "
        "the record or the registry changed, so the run is not reinterpretable under the current contract")
  versions = record.get("prompt_versions") or {}
  expected = {"editor": contract.editor_prompt_version, "reviewer": contract.reviewer_prompt_version}
  if versions != expected:
    raise ReplayError(
        f"the run record's prompt versions {versions} do not match the {contract.name!r} contract's "
        f"{expected}; the record changed after the run")
  return contract


def resolve_variant(name: str) -> ExperimentContract:
  """The named variant contract, or a visible failure listing the defined names."""
  contract = VARIANTS.get(name)
  if contract is None:
    raise ReplayError(f"unknown experiment variant {name!r}; defined variants are {', '.join(VARIANT_ORDER)}")
  return contract


# --- the five variants ---------------------------------------------------------------------------

VARIANT_BASELINE = "baseline-original-flow"
VARIANT_RATIONALE_HIDDEN = "rationale-hidden-review"
VARIANT_WHOLE_ENTRY = "whole-entry-review"
VARIANT_APPROVED_FEEDBACK = "approved-edit-feedback"
VARIANT_COMBINED = "combined-proposed-design"

_SELECTOR_EDITOR_RAW = "memory-experiment-editor-selector-raw-history-v1"
_SELECTOR_EDITOR_SELECTED = "memory-experiment-editor-selector-selected-v1"
_EDITOR_COMBINED = "memory-experiment-editor-combined-v1"
_REVIEWER_TRIM_RAW_VISIBLE = "memory-experiment-reviewer-trim-raw-history-visible-v1"
_REVIEWER_TRIM_RAW_HIDDEN = "memory-experiment-reviewer-trim-raw-history-hidden-v1"
_REVIEWER_TRIM_SELECTED_VISIBLE = "memory-experiment-reviewer-trim-selected-visible-v1"
_REVIEWER_WHOLE_ENTRY_RAW_VISIBLE = "memory-experiment-reviewer-whole-entry-raw-history-visible-v1"
_REVIEWER_COMBINED = "memory-experiment-reviewer-combined-v1"

_VARIANT_VERSION = 1


def _selector_build(feedback_view: str) -> Callable:
  return lambda manifest, theme, selections: build_selector_request(
      manifest, theme, selections, feedback_view=feedback_view)


def _selector_reviewer_build(feedback_view: str, rationale_visible: bool) -> Callable:
  return lambda manifest, theme, selections, editor_output: build_selector_reviewer_request(
      manifest, theme, selections, editor_output, feedback_view=feedback_view, rationale_visible=rationale_visible)


def _variant(
    *,
    name: str,
    title: str,
    editor_stage: str,
    reviewer_stage: str,
    feedback_view: str,
    rationale_visibility: str,
    reviewer_capability: str,
    editor_prompt_version: str,
    reviewer_prompt_version: str,
    editor_system: str,
    reviewer_system: str,
    changes_vs_baseline: tuple[str, ...],
    notes: tuple[str, ...],
) -> ExperimentContract:
  return ExperimentContract(
      name=name,
      title=title,
      version=_VARIANT_VERSION,
      editor_stage=editor_stage,
      reviewer_stage=reviewer_stage,
      feedback_view=feedback_view,
      rationale_visibility=rationale_visibility,
      reviewer_capability=reviewer_capability,
      editor_prompt_version=editor_prompt_version,
      reviewer_prompt_version=reviewer_prompt_version,
      editor_system=editor_system,
      reviewer_system=reviewer_system,
      build_editor_request=_selector_build(feedback_view) if editor_stage == "baseline-selector" else
      (exchange.build_editor_request),
      build_reviewer_request=_selector_reviewer_build(feedback_view, rationale_visibility == RATIONALE_VISIBLE)
      if reviewer_stage != "proposed-design" else exchange.build_reviewer_request,
      parse_editor_output=parse_selector_output if editor_stage == "baseline-selector" else exchange.parse_model_output,
      # Reviewers never write proofs in any variant, so every reviewer parses with the plain v3 spec.
      parse_reviewer_output=exchange.parse_model_output,
      editor_errors=_selector_editor_errors(feedback_view) if editor_stage == "baseline-selector" else
      (_narrowed_v3_errors(feedback_view)),
      reviewer_errors=(
          _trim_reviewer_errors(feedback_view)
          if reviewer_capability == TRIM_ONLY else _narrowed_v3_errors(feedback_view)),
      feedback_refs=_feedback_refs_all if feedback_view == RAW_HISTORY_VIEW else _feedback_refs_selected,
      changes_vs_baseline=changes_vs_baseline,
      notes=notes,
  )


VARIANT_ORDER = (
    VARIANT_BASELINE,
    VARIANT_RATIONALE_HIDDEN,
    VARIANT_WHOLE_ENTRY,
    VARIANT_APPROVED_FEEDBACK,
    VARIANT_COMBINED,
)

VARIANTS: dict[str, ExperimentContract] = {
    contract.name: contract for contract in (
        _variant(
            name=VARIANT_BASELINE,
            title="Adapted original judgment flow (baseline)",
            editor_stage="baseline-selector",
            reviewer_stage="baseline-trim",
            feedback_view=RAW_HISTORY_VIEW,
            rationale_visibility=RATIONALE_VISIBLE,
            reviewer_capability=TRIM_ONLY,
            editor_prompt_version=_SELECTOR_EDITOR_RAW,
            reviewer_prompt_version=_REVIEWER_TRIM_RAW_VISIBLE,
            editor_system=SELECTOR_RAW_HISTORY_SYSTEM,
            reviewer_system=TRIM_RAW_HISTORY_VISIBLE_SYSTEM,
            changes_vs_baseline=(),
            notes=(
                BASELINE_SOURCE_ANCHORS,
                "The original production curation judgment flow on frozen inputs: candidate-by-candidate, "
                "merge-first selection whose handoff carries the three Action/Home/Brevity proof lines, and "
                "a line-gating reviewer that may remove text, reject new entries, or restore the base but "
                "writes no new entry prose.",
            ),
        ),
        _variant(
            name=VARIANT_RATIONALE_HIDDEN,
            title="Rationale hidden from review",
            editor_stage="baseline-selector",
            reviewer_stage="baseline-trim",
            feedback_view=RAW_HISTORY_VIEW,
            rationale_visibility=RATIONALE_HIDDEN,
            reviewer_capability=TRIM_ONLY,
            editor_prompt_version=_SELECTOR_EDITOR_RAW,
            reviewer_prompt_version=_REVIEWER_TRIM_RAW_HIDDEN,
            editor_system=SELECTOR_RAW_HISTORY_SYSTEM,
            reviewer_system=TRIM_RAW_HISTORY_HIDDEN_SYSTEM,
            changes_vs_baseline=(
                "The reviewer request (initial and repair) no longer carries the selector's handoff: its "
                "disposition rows and the three Action/Home/Brevity proof lines are withheld, so the "
                "reviewer judges the proposed text on the evidence alone.",),
            notes=(),
        ),
        _variant(
            name=VARIANT_WHOLE_ENTRY,
            title="Whole-entry editing and review",
            editor_stage="baseline-selector",
            reviewer_stage="whole-entry",
            feedback_view=RAW_HISTORY_VIEW,
            rationale_visibility=RATIONALE_VISIBLE,
            reviewer_capability=WHOLE_ENTRY,
            editor_prompt_version=_SELECTOR_EDITOR_RAW,
            reviewer_prompt_version=_REVIEWER_WHOLE_ENTRY_RAW_VISIBLE,
            editor_system=SELECTOR_RAW_HISTORY_SYSTEM,
            reviewer_system=WHOLE_ENTRY_RAW_HISTORY_VISIBLE_SYSTEM,
            changes_vs_baseline=(
                "The reviewer may rewrite proposed entries as wholes with its own complete text, delete, "
                "keep, or restore — the redesigned reviewer's authority — instead of being limited to "
                "removing lines from the selector's text.",),
            notes=(),
        ),
        _variant(
            name=VARIANT_APPROVED_FEEDBACK,
            title="Selected approved-edit feedback",
            editor_stage="baseline-selector",
            reviewer_stage="baseline-trim",
            feedback_view=SELECTED_STRUCTURED_VIEW,
            rationale_visibility=RATIONALE_VISIBLE,
            reviewer_capability=TRIM_ONLY,
            editor_prompt_version=_SELECTOR_EDITOR_SELECTED,
            reviewer_prompt_version=_REVIEWER_TRIM_SELECTED_VISIBLE,
            editor_system=SELECTOR_SELECTED_SYSTEM,
            reviewer_system=TRIM_SELECTED_VISIBLE_SYSTEM,
            changes_vs_baseline=(
                "The raw comment history is replaced by the existing relevance selection with structured "
                "original-comment and approved before/after examples, supplied to both the selector and "
                "the reviewer as in the new design; only the selected comments (and their exposed "
                "approved-change refs) are citable.",),
            notes=(
                "Absent approved after-text stays absent: a comment without an approved revision arrives "
                "without one, and an approved creation or deletion keeps its empty side verbatim.",),
        ),
        _variant(
            name=VARIANT_COMBINED,
            title="Combined proposed design",
            editor_stage="proposed-design",
            reviewer_stage="proposed-design",
            feedback_view=SELECTED_STRUCTURED_VIEW,
            rationale_visibility=RATIONALE_HIDDEN,
            reviewer_capability=WHOLE_ENTRY,
            editor_prompt_version=_EDITOR_COMBINED,
            reviewer_prompt_version=_REVIEWER_COMBINED,
            editor_system=COMBINED_EDITOR_SYSTEM,
            reviewer_system=COMBINED_REVIEWER_SYSTEM,
            changes_vs_baseline=(
                "All three interventions together, as the approved design proposes: whole-entry editing "
                "and review, the selector's rationale withheld from the reviewer, and the selected "
                "approved-edit feedback view supplied to both stages.",),
            notes=(),
        ),
    )
}
