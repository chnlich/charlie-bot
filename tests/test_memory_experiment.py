"""Tests for the fixed-input variant experiment (src/core/memory_replay/variants.py + experiment.py).

Every fixture is synthetic and portable. The fake transports answer each stage call from a
script and record every request, so the tests assert the observable boundary: what each variant's
stages actually received, which capabilities each reviewer validator accepts or rejects, what the
run records and summaries carry, and what the comparison refuses when a record or bundle changed.
"""

import json
import sys
from pathlib import Path

import pytest
import test_memory_replay as base
import yaml

from src.core.config import CharlieBotConfig
from src.core.memory_replay import CompareOptions, ReplayError, ReplayOptions, run_comparison, run_replay, variants
from src.core.memory_replay.experiment import ExperimentOptions, run_experiment
from src.core.memory_replay.identity import sha256_hex

# --- synthetic frozen corpus ----------------------------------------------------

EXTRA_LINE = "- Extra tuning note the selector merged in."
EDITOR_TEXT = base.entry_text([base.MECHANISM_LINE, EXTRA_LINE])
TRIMMED_TEXT = base.entry_text([base.MECHANISM_LINE])
NEW_PROSE_TEXT = base.entry_text([base.MECHANISM_LINE, "- The reviewer wrote a fresh line of its own."])
WHOLE_REWRITE_TEXT = base.entry_text(["- Rewrite: one threshold gates eviction and alerts; tune it on warm replay."])
NEW_PATH = "entries/render/extra-note.md"
NEW_ENTRY_TEXT = base.entry_text(["- A brand-new admission the selector proposed."])

PROOFS = {
    "action": "PROOF-ACTION-MARKER: tuning reads the threshold on every warm-up replay.",
    "home": "PROOF-HOME-MARKER: the runbook owns values, the store owns the mechanism rule.",
    "brevity": "PROOF-BREVITY-MARKER: trimmed the warm-up instance line from the merged draft.",
}

UNASSIGNED_DOC = "Unassigned owning document text UNASSIGNED-DOC-MARKER\n"
UNSELECTED_COMMENT = "Please prefer the darker plot color palette. UNSELECTED-COMMENT-MARKER"


def feedback_rich_manifest_dict() -> dict:
  """The base corpus plus an unassigned document, an unselected comment, and an approved deletion."""
  manifest = base.base_manifest_dict()
  manifest["sources"].append({"ref": "doc-unassigned", "kind": "document", "text": UNASSIGNED_DOC})
  manifest["feedback_examples"].append(
      {
          "comment_event": "fb-002",
          "comment_text": UNSELECTED_COMMENT,
          "tags": [],
          "approved_change": None,  # comment only: no approved revision exists
      })
  manifest["feedback_examples"].append(
      {
          "comment_event": "fb-003",
          "comment_text": "Instance names leave; the mechanism stays. EMPTY-AFTER-COMMENT-MARKER",
          "tags": ["instance-names-out"],
          "approved_change":
              {
                  "approved_change_ref": "approved-003",
                  "before": base.ENTRY_WITH_INSTANCE,
                  "after": "",  # an approved deletion: the empty side is the real content
              },
      })
  manifest["feedback_examples"].append(
      {
          "comment_event": "fb-004",
          "comment_text": "Keep the tuning range visible. NO-APPROVED-REVISION-MARKER",
          "tags": ["mechanism-in"],
          "approved_change": None,  # selected, but the user never approved a revision for it
      })
  return manifest


def write_manifest(tmp_path: Path, data: dict | None = None, name: str = "manifest.yaml") -> Path:
  p = tmp_path / name
  p.write_text(
      yaml.safe_dump(data if data is not None else feedback_rich_manifest_dict(), sort_keys=False), encoding="utf-8")
  return p


# --- scripted responses ---------------------------------------------------------


def selector_row(
    ref: str = "capture-eviction",
    outcome: str = "propose",
    paths: list[str] | None = None,
    proofs: dict | None = -1) -> dict:
  row = {
      "source_ref": ref,
      "outcome": outcome,
      "paths": paths if paths is not None else ["entries/render/cache-eviction.md"],
      "reason": "merge-first revise of the covered entry",
  }
  if proofs != -1:
    if proofs is not None:
      row["proofs"] = dict(proofs)
  elif outcome == "propose":
    row["proofs"] = dict(PROOFS)
  return row


def selector_json(entries: list[dict], candidates: list[dict]) -> str:
  return json.dumps({"entries": entries, "candidates": candidates})


def reviewer_row(
    ref: str = "capture-eviction",
    outcome: str = "propose",
    paths: list[str] | None = None,
    reason: str = "removed the failing instance line") -> dict:
  return {
      "source_ref": ref,
      "outcome": outcome,
      "paths": paths if paths is not None else ["entries/render/cache-eviction.md"],
      "reason": reason,
  }


SELECTOR_EDITOR_RESPONSE = selector_json(
    [
        {
            "action": "rewrite",
            "path": "entries/render/cache-eviction.md",
            "text": EDITOR_TEXT,
            "source_refs": ["capture-eviction", "entry-cache-eviction"],
            "reason": "merged the capture",
        }
    ], [selector_row()])

TRIM_ACCEPT_RESPONSE = selector_json(
    [
        {
            "action": "rewrite",
            "path": "entries/render/cache-eviction.md",
            "text": TRIMMED_TEXT,
            "source_refs": ["capture-eviction", "entry-cache-eviction"],
            "reason": "removed the failing instance line",
        }
    ], [reviewer_row()])

NEW_PROSE_RESPONSE = selector_json(
    [
        {
            "action": "rewrite",
            "path": "entries/render/cache-eviction.md",
            "text": NEW_PROSE_TEXT,
            "source_refs": ["capture-eviction"],
            "reason": "rewrote the entry",
        }
    ], [reviewer_row()])

WHOLE_REWRITE_RESPONSE = selector_json(
    [
        {
            "action": "rewrite",
            "path": "entries/render/cache-eviction.md",
            "text": WHOLE_REWRITE_TEXT,
            "source_refs": ["capture-eviction"],
            "reason": "rewrote the entry whole",
        }
    ], [reviewer_row()])

RESTORE_RESPONSE = selector_json([], [reviewer_row(outcome="no_change", paths=[])])

SELECTOR_NEW_ADMISSION_RESPONSE = selector_json(
    [
        {
            "action": "new",
            "path": NEW_PATH,
            "text": NEW_ENTRY_TEXT,
            "source_refs": ["capture-eviction"],
            "reason": "no theme coverage",
        }
    ], [selector_row(paths=[NEW_PATH])])

# A second theme, so per-theme provenance and per-theme content equality are observable.
ALERTS_ENTRY_TEXT = (
    "---\nscope: user\ntopic: render\naudience: master, worker\n"
    "title: alert threshold: page only on sustained breaches\n---\n"
    "- The alert fires only when the metric breaches for three consecutive windows; single-window spikes "
    "stay silent.\n")
ALERT_CAPTURE_TEXT = (
    "# render alert threshold (capture)\n\nSingle-window spikes paged the on-call twice this week; the "
    "alert should require three consecutive breaching windows.\n")


def two_theme_manifest_dict() -> dict:
  """The base corpus plus an alerts entry, an alerts capture, and a second theme."""
  manifest = base.base_manifest_dict()
  manifest["sources"].append(
      {
          "ref": "entry-alerts",
          "kind": "entry",
          "path": "entries/render/alert-threshold.md",
          "text": ALERTS_ENTRY_TEXT,
      })
  manifest["sources"].append({"ref": "capture-alerts", "kind": "candidate", "text": ALERT_CAPTURE_TEXT})
  manifest["themes"]["alerts"] = {
      "principles": ["sustained-breach-only"],
      "candidate_refs": ["capture-alerts"],
      "entry_refs": ["entry-alerts"],
      "document_refs": [],
  }
  return manifest


def alerts_editor_json(text: str) -> str:
  return selector_json(
      [
          {
              "action": "rewrite",
              "path": "entries/render/alert-threshold.md",
              "text": text,
              "source_refs": ["capture-alerts", "entry-alerts"],
              "reason": "sustained-breach rule folded in",
          }
      ], [selector_row(ref="capture-alerts", paths=["entries/render/alert-threshold.md"])])


ALERTS_EDITOR_TEXT_A = base.entry_text(
    [
        "- The alert fires only when the metric breaches for three consecutive windows; single-window spikes "
        "stay silent.",
        "- Page the on-call only after the third breaching window closes.",
    ])
ALERTS_EDITOR_TEXT_B = base.entry_text(
    [
        "- The alert fires only when the metric breaches for three consecutive windows; single-window spikes "
        "stay silent, and the window count is owned by the runbook.",
    ])
ALERTS_EDITOR_RESPONSE_A = alerts_editor_json(ALERTS_EDITOR_TEXT_A)
ALERTS_EDITOR_RESPONSE_B = alerts_editor_json(ALERTS_EDITOR_TEXT_B)


class VariantScriptedTransport:
  """Answers editor-stage and reviewer-stage prompts from a script and records every request."""

  def __init__(
      self,
      *,
      editor_response: str | None = None,
      reviewer_response: str | None = None,
      die_on_call: int | None = None,
      editor_responses: list[str] | None = None,
      reviewer_responses: list[str] | None = None):
    # A single scripted response repeats for every stage call of its kind; an explicit list is
    # consumed in order and runs out loudly. One editor queue serves every editor scope: the
    # candidate-merge selector and the whole-entry editor share the same proof contract, so the
    # same scripted bytes are a valid editor response for any variant.
    self.editor_queue = [editor_response] if editor_responses is None else list(editor_responses)
    self.reviewer_queue = [reviewer_response] if reviewer_responses is None else list(reviewer_responses)
    self.editor_repeat = editor_responses is None
    self.reviewer_repeat = reviewer_responses is None
    self.die_on_call = die_on_call
    self.calls: list[dict] = []

  def complete(self, *, system: str, user: str):
    from src.core.memory_replay.transport import TransportResult

    self.calls.append({"system": system, "user": user})
    if self.die_on_call is not None and len(self.calls) == self.die_on_call:
      raise ReplayError("model endpoint returned HTTP 502: bad gateway")
    if "reviewer stage" in system:
      if not self.reviewer_queue:
        raise AssertionError("scripted transport ran out of reviewer responses")
      text = self.reviewer_queue.pop(0) if not self.reviewer_repeat else self.reviewer_queue[0]
    else:  # every editor stage: the candidate-merge selector and the whole-entry editor alike
      if not self.editor_queue:
        raise AssertionError("scripted transport ran out of editor responses")
      text = self.editor_queue.pop(0) if not self.editor_repeat else self.editor_queue[0]
    return TransportResult(text=text, model="fake-model", prompt_tokens=100, output_tokens=42, latency_ms=5)


def replay_cfg(tmp_path: Path) -> CharlieBotConfig:
  return base.replay_cfg(tmp_path)


def run_variant(
    tmp_path: Path,
    variant: str,
    *,
    transport: VariantScriptedTransport | None = None,
    manifest_path: Path | None = None,
    output_dir: Path | None = None,
):
  contract = variants.resolve_variant(variant)
  transport = transport or VariantScriptedTransport(
      editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=TRIM_ACCEPT_RESPONSE)
  outcome = run_replay(
      ReplayOptions(
          manifest=manifest_path or write_manifest(tmp_path),
          output_dir=output_dir or tmp_path / "out",
          backend="fake-clc",
          mode="editor-review"),
      cfg=replay_cfg(tmp_path),
      transport_factory=lambda: transport,
      contract=contract)
  return outcome, transport


def run_variant_expect_failure(tmp_path: Path, variant: str, *, transport, output_dir: Path, match: str) -> dict:
  """A run that must fail visibly; returns the preserved failed record."""
  with pytest.raises(ReplayError, match=match):
    run_variant(tmp_path, variant, transport=transport, output_dir=output_dir)
  return base.run_record(next(iter((output_dir / "runs").iterdir())))


def payload_of(request_text: str) -> dict:
  return base.evidence_payload(request_text)


def stage_calls(transport: VariantScriptedTransport, stage: str) -> list[dict]:
  if stage == "reviewer":
    return [call for call in transport.calls if "reviewer stage" in call["system"]]
  return [call for call in transport.calls if "reviewer stage" not in call["system"]]


def editor_calls(transport: VariantScriptedTransport) -> list[dict]:
  """The transport's actual editor-stage calls, whatever the variant's editor scope is."""
  return stage_calls(transport, "editor")


def repair_original_request(repair_text: str) -> str:
  """The original request embedded in a bounded repair request's '## Original request' section."""
  head, _, rest = repair_text.partition("## Original request\n\n")
  assert head == "" and rest, "the repair request embeds the original request"
  original, _, tail = rest.partition("\n\n## Your previous response\n")
  assert tail, "the repair request carries the previous response and the errors"
  return original


def snapshot_files(root: Path) -> dict[Path, bytes]:
  return {path.relative_to(root): path.read_bytes() for path in sorted(root.rglob("*")) if path.is_file()}


# --- the variant matrix ---------------------------------------------------------


def test_variant_matrix_changes_only_the_declared_dimensions() -> None:
  # (editor stage, reviewer stage, editing/review scope, feedback view, rationale visibility)
  expected = {
      "baseline-original-flow":
          ("candidate-merge-selector", "trim-review", "candidate-merge", "raw-history", "visible"),
      "rationale-hidden-review":
          ("candidate-merge-selector", "trim-review", "candidate-merge", "raw-history", "hidden"),
      "whole-entry-review": ("whole-entry-editor", "whole-entry-reviewer", "whole-entry", "raw-history", "visible"),
      "approved-edit-feedback":
          ("candidate-merge-selector", "trim-review", "candidate-merge", "selected-structured", "visible"),
      "combined-proposed-design":
          ("whole-entry-editor", "whole-entry-reviewer", "whole-entry", "selected-structured", "hidden"),
  }
  assert tuple(variants.VARIANT_ORDER) == tuple(expected)
  for name, (editor_stage, reviewer_stage, scope, view, rationale) in expected.items():
    contract = variants.resolve_variant(name)
    assert (
        contract.editor_stage, contract.reviewer_stage, contract.entry_scope, contract.feedback_view,
        contract.rationale_visibility) == (editor_stage, reviewer_stage, scope, view, rationale), name
    assert contract.version == variants.VARIANT_DEFINITION_VERSION == 2, name
    assert contract.identity_payload() == {
        "name": name,
        "version": 2,
        "entry_scope": scope,
        "rationale_visibility": rationale,
        "feedback_view": view,
    }, name


def test_baseline_definition_anchors_the_pinned_source_prompts() -> None:
  contract = variants.resolve_variant("baseline-original-flow")
  notes = "\n".join(contract.notes)
  assert variants.BASELINE_SOURCE_REVISION in notes
  assert "prompts/cron/memory_curator/memory_selector.md" in notes
  assert "prompts/cron/memory_curator/memory_reviewer.md" in notes
  assert "bdefc53d138c73e03d2f5f61c3264ea7ddfd2b5b51ad2bac10c81a5726d8f59c" in notes
  assert "73a2c360667c6c4186f29bcd3e02a0a948cffcbcb5a4bf5bdfad558bff52c147" in notes


def test_every_variant_prompt_states_the_english_memory_rule() -> None:
  for name in variants.VARIANT_ORDER:
    contract = variants.resolve_variant(name)
    for system in (contract.editor_system, contract.reviewer_system):
      assert variants.LANGUAGE_RULE.strip() in system, name


def test_single_interventions_change_only_their_declared_dimension(tmp_path: Path) -> None:
  """The request-level matrix: each intervention moves exactly its own declared dimension."""
  requests: dict[str, dict[str, str]] = {}
  for name in variants.VARIANT_ORDER:
    transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
    outcome, _ = run_variant(
        tmp_path,
        name,
        transport=transport,
        output_dir=tmp_path / f"out-{name}",
        manifest_path=write_manifest(tmp_path, name=f"{name}.yaml"))
    assert outcome.run_dir is not None
    requests[name] = {
        "editor": transport.calls[0]["user"],
        "reviewer": transport.calls[1]["user"],
    }
    assert len(transport.calls) == 2, name

  # feedback view: the raw-history editor requests are byte-identical across variants 1-3 and
  # carry the whole comment pool with no approved revisions; the selected-view editor requests
  # (variants 4 and 5) are byte-identical to each other and carry only the selection.
  raw_editors = [
      requests[name]["editor"] for name in ("baseline-original-flow", "rationale-hidden-review", "whole-entry-review")
  ]
  assert raw_editors[0] == raw_editors[1] == raw_editors[2]
  selected_editors = [requests["approved-edit-feedback"]["editor"], requests["combined-proposed-design"]["editor"]]
  assert selected_editors[0] == selected_editors[1]
  raw_payload = payload_of(raw_editors[0])
  selected_payload = payload_of(selected_editors[0])
  assert "feedback_history" in raw_payload and "feedback" not in raw_payload
  assert "feedback" in selected_payload and "feedback_history" not in selected_payload
  assert {row["comment_event"] for row in raw_payload["feedback_history"]} == {"fb-001", "fb-002", "fb-003", "fb-004"}
  assert all("approved_change" not in json.dumps(row) for row in raw_payload["feedback_history"])
  assert {row["comment_event"] for row in selected_payload["feedback"]
         } == {"fb-001", "fb-003", "fb-004"}, ("the relevance selection, not the pool, is the selected view")
  for key in set(raw_payload) - {"feedback_history"}:
    assert raw_payload[key] == selected_payload.get(key), (
        f"the feedback view is the only evidence difference; {key} drifted")
  # the rendered instructions name the feedback keys the requests actually carry, in every role.
  for name, view_key in (("baseline-original-flow", "feedback_history"), ("approved-edit-feedback", "feedback")):
    contract = variants.resolve_variant(name)
    assert f'"{view_key}"' in contract.editor_system and f'"{view_key}"' in contract.reviewer_system
    other = "feedback" if view_key == "feedback_history" else "feedback_history"
    assert f'"{other}" (' not in contract.editor_system and f'"{other}" (' not in contract.reviewer_system

  # rationale visibility: identical editor request and editor prompt; the reviewer request differs
  # only by the handoff dispositions, the reviewer prompt only by the handoff paragraph.
  assert requests["baseline-original-flow"]["editor"] == requests["rationale-hidden-review"]["editor"]
  assert variants.resolve_variant("baseline-original-flow").editor_system == variants.resolve_variant(
      "rationale-hidden-review").editor_system
  visible_payload = payload_of(requests["baseline-original-flow"]["reviewer"])
  hidden_payload = payload_of(requests["rationale-hidden-review"]["reviewer"])
  assert "dispositions" in visible_payload["editor_proposals"]
  assert set(hidden_payload["editor_proposals"]) == {"entries"}
  assert visible_payload["editor_proposals"]["entries"] == hidden_payload["editor_proposals"]["entries"]
  for key in set(visible_payload) - {"editor_proposals"}:
    assert visible_payload[key] == hidden_payload[key]
  visible_contract = variants.resolve_variant("baseline-original-flow")
  hidden_contract = variants.resolve_variant("rationale-hidden-review")
  assert hidden_contract.reviewer_system.replace(
      variants._HANDOFF_HIDDEN, variants._HANDOFF_VISIBLE) == visible_contract.reviewer_system

  # editing/review scope: byte-identical requests to the baseline (same evidence, same builder);
  # the editor prompt's decision unit and the reviewer prompt's authority change, nothing else.
  assert requests["whole-entry-review"]["editor"] == requests["baseline-original-flow"]["editor"]
  assert requests["whole-entry-review"]["reviewer"] == requests["baseline-original-flow"]["reviewer"]
  whole_contract = variants.resolve_variant("whole-entry-review")
  assert whole_contract.editor_system != visible_contract.editor_system, "the decision unit changed"
  assert whole_contract.reviewer_system != visible_contract.reviewer_system, "the reviewer authority changed"
  # ... while the proof contract and the response schema stay the shared editor ones.
  assert variants._EDITOR_PROOFS in whole_contract.editor_system
  assert variants._EDITOR_PROOFS in visible_contract.editor_system
  assert variants._EDITOR_SHAPE in whole_contract.editor_system
  assert whole_contract.parse_editor_output is visible_contract.parse_editor_output

  # combined: hidden rationale (like variant 2) over the selected feedback view (like variant 4).
  combined_payload = payload_of(requests["combined-proposed-design"]["reviewer"])
  selected_reviewer_payload = payload_of(requests["approved-edit-feedback"]["reviewer"])
  assert set(combined_payload["editor_proposals"]) == {"entries"}
  assert set(combined_payload) - {"editor_proposals"} == set(selected_reviewer_payload) - {"editor_proposals"}, (
      "the combined reviewer's evidence is the selected view, identical to variant 4's")
  assert combined_payload["feedback"] == selected_reviewer_payload["feedback"]
  assert combined_payload["editor_proposals"]["entries"] == hidden_payload["editor_proposals"]["entries"]


RAW_VIEW_BLOCKS = (
    variants._FEEDBACK_RAW_HISTORY,
    variants._structure_block(variants.RAW_HISTORY_VIEW),
    variants._citations_block(variants.RAW_HISTORY_VIEW),
    variants._dispositions_block(variants.RAW_HISTORY_VIEW),
)
SELECTED_VIEW_BLOCKS = (
    variants._FEEDBACK_SELECTED,
    variants._structure_block(variants.SELECTED_STRUCTURED_VIEW),
    variants._citations_block(variants.SELECTED_STRUCTURED_VIEW),
    variants._dispositions_block(variants.SELECTED_STRUCTURED_VIEW),
)


def _prompt_blocks(system: str) -> list[str]:
  """The composed instruction blocks; no block contains the '\n\n' block separator."""
  return system.split("\n\n")


def test_combined_condition_is_exactly_the_three_declared_dimensions() -> None:
  """The combined arm composes the declared dimensions; nothing rides only in it."""
  baseline = variants.resolve_variant("baseline-original-flow")
  whole = variants.resolve_variant("whole-entry-review")
  combined = variants.resolve_variant("combined-proposed-design")
  selected = variants.resolve_variant("approved-edit-feedback")

  # The combined editor is the whole-entry editor over the selected view: a block-by-block diff
  # against the whole-entry editor may differ only in the declared feedback view's blocks.
  whole_blocks = _prompt_blocks(whole.editor_system)
  combined_blocks = _prompt_blocks(combined.editor_system)
  assert len(whole_blocks) == len(combined_blocks)
  diffs = [
      (old_block, new_block) for old_block, new_block in zip(whole_blocks, combined_blocks) if old_block != new_block
  ]
  assert {old_block for old_block, _ in diffs} <= set(RAW_VIEW_BLOCKS), diffs
  assert {new_block for _, new_block in diffs} <= set(SELECTED_VIEW_BLOCKS), diffs
  assert diffs, "the declared feedback view must actually differ"

  # The proof contract, the decision unit, and the schema are the shared ones in every editor arm.
  for contract in (baseline, whole, combined, selected):
    assert variants._EDITOR_PROOFS in contract.editor_system, contract.name
    assert variants._EDITOR_SHAPE in contract.editor_system, contract.name
    flow = variants._WHOLE_ENTRY_FLOW if contract.entry_scope == variants.ENTRY_SCOPE_WHOLE_ENTRY else (
        variants._CANDIDATE_MERGE_FLOW)
    assert flow in contract.editor_system, contract.name
    assert contract.parse_editor_output is whole.parse_editor_output, contract.name
  assert variants._EDITOR_SHAPE not in combined.reviewer_system, "reviewers parse the plain v3 shape"
  assert variants._EDITOR_PROOFS not in combined.reviewer_system, "reviewers write no proofs"

  # The combined review is the whole-entry authority with the handoff withheld.
  assert variants._HANDOFF_HIDDEN in combined.reviewer_system
  assert variants._HANDOFF_VISIBLE not in combined.reviewer_system
  assert variants._WHOLE_ENTRY_AUTHORITY in combined.reviewer_system
  hidden_trim = variants.resolve_variant("rationale-hidden-review")
  assert combined.reviewer_errors is not hidden_trim.reviewer_errors, (
      "the combined review is a whole-entry review, not a trim review over hidden rationale")


def test_editor_scope_contrasts_the_task_framing_of_the_two_editor_units() -> None:
  """Candidate-driven vs whole-entry framing, without inventing a new text permission."""
  baseline = variants.resolve_variant("baseline-original-flow")
  whole = variants.resolve_variant("whole-entry-review")
  assert "curate staged candidates one by one" in baseline.editor_system
  assert "merge-first" in baseline.editor_system
  assert "curate staged candidates one by one" not in whole.editor_system
  assert "read the theme's base entries, all of its candidates, and its feedback view together" in whole.editor_system
  assert "one decision over the theme's base, candidates, and feedback together" in whole.editor_system
  # The baseline keeps its original merge-time whole-entry trimming; whole-entry editing is a
  # decision-unit change, not a newly granted text permission.
  assert "Trimming a merged entry down to what a future action needs — whole lines included — is the "
  "original merge-time behavior, permitted by the guideline." in baseline.editor_system
  assert "merge-time trimming of a whole entry is original behavior the guideline permits" in whole.editor_system
  # The reviewer sides contrast the same way: trim-only gating vs whole-entry authority.
  assert "You write no new entry prose" in baseline.reviewer_system
  assert "You write no new entry prose" not in whole.reviewer_system
  assert "you own the final content" in whole.reviewer_system


# --- the three proofs are model output ------------------------------------------


def test_selector_proofs_are_recorded_model_output_and_hand_off_to_the_reviewer(tmp_path: Path) -> None:
  outcome, transport = run_variant(tmp_path, "baseline-original-flow")
  record = base.run_record(outcome.run_dir)
  proofs_rows = [row for row in record["editor_dispositions"] if row.get("proofs")]
  assert proofs_rows and proofs_rows[0]["proofs"] == PROOFS, "the proofs are the model's own rows"
  raw_response = (outcome.run_dir / "raw" / "editor-eviction.attempt-1.response.txt").read_text(encoding="utf-8")
  assert PROOFS["action"] in raw_response, "the handoff proofs come from the recorded model response"
  reviewer_request = transport.calls[1]["user"]
  for proof in PROOFS.values():
    assert proof in reviewer_request, "the visible-rationale reviewer sees the actual editor proof text"
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  assert "proofs" not in json.dumps(proposal), "the public proposal schema never grows"
  assert any(variants.BASELINE_SOURCE_REVISION in note for note in record["variant"]["notes"]), (
      "the run record carries the pinned source anchors for audit")


def test_hidden_rationale_excludes_proofs_from_the_initial_and_repair_requests(tmp_path: Path) -> None:
  transport = VariantScriptedTransport(
      editor_response=SELECTOR_EDITOR_RESPONSE,
      reviewer_responses=[NEW_PROSE_RESPONSE, TRIM_ACCEPT_RESPONSE],  # force one bounded reviewer re-ask
  )
  outcome, _ = run_variant(tmp_path, "rationale-hidden-review", transport=transport)
  record = base.run_record(outcome.run_dir)
  reviewer_calls = [call for call in record["calls"] if call["role"] == "reviewer"]
  assert len(reviewer_calls) == 2, "the mechanically invalid first response was re-asked once"
  for call in reviewer_calls:
    request = (outcome.run_dir / call["request_file"]).read_text(encoding="utf-8")
    for proof in PROOFS.values():
      assert proof not in request, f"{call['request_file']} must not leak the withheld rationale"
    assert '"dispositions"' not in request, (
        f"{call['request_file']} (initial or repair) must not carry the withheld handoff")
    if call["attempt"] == 2:
      payload = payload_of(repair_original_request(request))
      assert "feedback_history" in payload and "feedback" not in payload, (
          "the repair request carries the same raw-history view the initial request carried")


def test_combined_editor_writes_the_shared_proofs_and_hides_them_from_review(tmp_path: Path) -> None:
  """The combined arm keeps the editor's proof contract; its review sees none of the rationale."""
  transport = VariantScriptedTransport(editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=TRIM_ACCEPT_RESPONSE)
  outcome, transport = run_variant(tmp_path, "combined-proposed-design", transport=transport)
  record = base.run_record(outcome.run_dir)
  proofs_rows = [row for row in record["editor_dispositions"] if row.get("proofs")]
  assert proofs_rows and proofs_rows[0]["proofs"] == PROOFS, "the combined editor writes the same three proofs"
  raw_response = (outcome.run_dir / "raw" / "editor-eviction.attempt-1.response.txt").read_text(encoding="utf-8")
  assert PROOFS["action"] in raw_response, "the proofs come from the recorded model response, not from code"
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  assert "proofs" not in json.dumps(proposal), "the audit proofs never enter the public proposal schema"
  reviewer_request = transport.calls[1]["user"]
  for proof in PROOFS.values():
    assert proof not in reviewer_request, "the combined review receives none of the author's rationale"
  payload = payload_of(reviewer_request)
  assert set(payload["editor_proposals"]) == {"entries"}
  assert "feedback" in payload and "feedback_history" not in payload


def test_propose_rows_require_complete_proofs_and_other_rows_forbid_them(tmp_path: Path) -> None:
  missing = selector_json(
      [
          {
              "action": "rewrite",
              "path": "entries/render/cache-eviction.md",
              "text": EDITOR_TEXT,
              "source_refs": ["capture-eviction"],
              "reason": "merged",
          }
      ], [
          {
              "source_ref": "capture-eviction",
              "outcome": "propose",
              "paths": ["entries/render/cache-eviction.md"],
              "reason": "merged",
          }
      ])
  record = run_variant_expect_failure(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(editor_responses=[missing, missing], reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-missing",
      match="admission proofs")
  assert record["status"] == "failed"

  partial = selector_json(
      [
          {
              "action": "rewrite",
              "path": "entries/render/cache-eviction.md",
              "text": EDITOR_TEXT,
              "source_refs": ["capture-eviction"],
              "reason": "merged",
          }
      ], [
          {
              "source_ref": "capture-eviction",
              "outcome": "propose",
              "paths": ["entries/render/cache-eviction.md"],
              "reason": "merged",
              "proofs": {
                  "action": "a",
                  "home": "",
                  "brevity": "b"
              },
          }
      ])
  record = run_variant_expect_failure(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(editor_responses=[partial, partial], reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-partial",
      match="empty 'home' proof line")
  assert record["status"] == "failed"

  stray = selector_json(
      [
          {
              "action": "rewrite",
              "path": "entries/render/cache-eviction.md",
              "text": EDITOR_TEXT,
              "source_refs": ["capture-eviction"],
              "reason": "merged",
          }
      ], [
          {
              "source_ref": "capture-eviction",
              "outcome": "no_change",
              "paths": [],
              "reason": "nothing to do",
              "proofs": dict(PROOFS),
          }
      ])
  record = run_variant_expect_failure(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(editor_responses=[stray, stray], reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-stray",
      match="only propose rows carry admission proofs")
  assert record["status"] == "failed"


@pytest.mark.parametrize("variant", list(variants.VARIANT_ORDER))
def test_every_editor_variant_requires_the_three_proofs(tmp_path: Path, variant: str) -> None:
  """The proof contract is the shared editor contract: no variant's editor may drop it."""
  missing = editor_with_refs("capture-eviction", proofs=False)
  record = run_variant_expect_failure(
      tmp_path,
      variant,
      transport=VariantScriptedTransport(editor_response=missing),
      output_dir=tmp_path / f"out-{variant}",
      match="admission proofs")
  assert record["status"] == "failed"
  editor_calls = [call for call in record["calls"] if call["role"] == "editor"]
  assert len(editor_calls) == 2, "the proof requirement is enforced again on the repair attempt"


# --- the run report describes what the stages actually received -----------------


def test_report_rationale_claim_matches_what_the_reviewer_request_carried(tmp_path: Path) -> None:
  """Visible rationale reports the handoff as reviewed; hidden rationale reports it as withheld.

  Each claim is checked against that variant's actual reviewer request, so the report cannot
  describe a handoff the request never carried — the reproduced defect had the visible baseline
  and the hidden variant render the same false "withheld" caption.
  """
  reports: dict[str, str] = {}
  reviewer_payloads: dict[str, dict] = {}
  for name in ("baseline-original-flow", "rationale-hidden-review"):
    transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
    outcome, _ = run_variant(
        tmp_path,
        name,
        transport=transport,
        manifest_path=write_manifest(tmp_path, name=f"{name}.yaml"),
        output_dir=tmp_path / f"out-{name}")
    reports[name] = (outcome.run_dir / "report.html").read_text(encoding="utf-8")
    reviewer_payloads[name] = payload_of(transport.calls[1]["user"])

  # Request evidence: the baseline reviewer request carried the handoff — disposition rows with
  # the three proofs — while the hidden variant's carried the proposed entries only.
  assert "dispositions" in reviewer_payloads["baseline-original-flow"]["editor_proposals"]
  assert PROOFS["action"] in json.dumps(reviewer_payloads["baseline-original-flow"]["editor_proposals"])
  assert set(reviewer_payloads["rationale-hidden-review"]["editor_proposals"]) == {"entries"}
  assert PROOFS["action"] not in json.dumps(reviewer_payloads["rationale-hidden-review"])
  # Each report describes its own request; the shared false caption is gone.
  assert "the reviewer request carried this handoff" in reports["baseline-original-flow"]
  assert "withheld from the reviewer" not in reports["baseline-original-flow"]
  assert "withheld from the reviewer" in reports["rationale-hidden-review"]
  assert "the reviewer request carried this handoff" not in reports["rationale-hidden-review"]


def test_raw_history_report_renders_the_pool_it_provided_not_the_selected_view(tmp_path: Path) -> None:
  """A raw-history run's report describes the whole comment pool the requests carried — unselected
  comment included — and never presents the bundled approved revisions as something the stages saw."""
  transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
  outcome, _ = run_variant(tmp_path, "baseline-original-flow", transport=transport)
  report = (outcome.run_dir / "report.html").read_text(encoding="utf-8")
  pool = payload_of(transport.calls[0]["user"])["feedback_history"]

  # The raw view exposed the whole pool: the report renders exactly the comments the request
  # carried, with their provenance ids, the unselected one included.
  assert {row["comment_event"] for row in pool} == {"fb-001", "fb-002", "fb-003", "fb-004"}
  assert "Feedback history (raw) (4 pool comments)" in report
  for row in pool:
    assert f"<p><code>{row['comment_event']}</code></p>" in report
    assert f"comment:\n{row['comment_text']}" in report
  assert "UNSELECTED-COMMENT-MARKER" in report, "the unselected pool comment was provided too"
  # The approved revisions were bundled but never exposed: the report says so and never renders
  # them as selected structured feedback with approved before/after texts.
  assert "the stages saw none of them" in report
  assert "approved change (ref" not in report
  assert "approved-001" not in report and "approved-003" not in report
  # The relevance-selection record stays available as folded audit data, labeled as not
  # model-visible.
  assert "not part of the raw view the stages saw" in report
  assert "score" in report


def test_selected_view_report_renders_exactly_the_provided_selection(tmp_path: Path) -> None:
  """The selected structured view's report renders the selection with its approved texts and marks
  pool comments the selection did not pick as never provided."""
  transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
  outcome, _ = run_variant(tmp_path, "approved-edit-feedback", transport=transport)
  report = (outcome.run_dir / "report.html").read_text(encoding="utf-8")
  provided = {row["comment_event"]: row for row in payload_of(transport.calls[0]["user"])["feedback"]}
  assert set(provided) == {"fb-001", "fb-003", "fb-004"}, "the relevance selection, not the pool"

  assert "Selected feedback (3)" in report
  for event, row in provided.items():
    assert f"comment:\n{row['comment_text']}" in report
    change = row["approved_change"]
    if change is not None:
      assert f"approved change (ref {change['approved_change_ref']})" in report
      assert f"--- before ---\n{change['before']}" in report
      assert f"--- after ---\n{change['after']}" in report
  # fb-003's approved deletion keeps its empty after side verbatim: nothing is invented for it.
  assert "--- after ---\n</pre>" in report
  # The unselected pool comment was never provided: its text stays out and its id is named as such.
  assert "UNSELECTED-COMMENT-MARKER" not in report
  assert "never provided to any stage: fb-002" in report


def test_report_escapes_pool_comment_and_proof_text(tmp_path: Path) -> None:
  """Pool comments and proof lines are arbitrary user/model text: the report escapes them.

  The requests carry the raw strings; the report must never let them become markup.
  """
  hostile_comment = "Hostile pool comment <script>alert('c')</script> fb-comment-marker"
  manifest = feedback_rich_manifest_dict()
  manifest["feedback_examples"][0]["comment_text"] = hostile_comment
  hostile_proofs = {
      "action": "proof action <script>alert('a')</script>",
      "home": "proof home <script>alert('h')</script>",
      "brevity": "proof brevity <script>alert('b')</script>",
  }
  editor = selector_json(
      [
          {
              "action": "rewrite",
              "path": "entries/render/cache-eviction.md",
              "text": EDITOR_TEXT,
              "source_refs": ["capture-eviction", "entry-cache-eviction"],
              "reason": "merged the capture",
          }
      ], [selector_row(proofs=hostile_proofs)])
  transport = VariantScriptedTransport(editor_response=editor, reviewer_response=TRIM_ACCEPT_RESPONSE)
  outcome, _ = run_variant(
      tmp_path, "baseline-original-flow", transport=transport, manifest_path=write_manifest(tmp_path, manifest))
  report = (outcome.run_dir / "report.html").read_text(encoding="utf-8")

  # The raw requests carried the raw strings: the pool comment reached the editor, the proofs the
  # visible-rationale reviewer.
  assert hostile_comment in transport.calls[0]["user"]
  assert "proof action <script>alert('a')</script>" in transport.calls[1]["user"]
  # The report escapes both.
  assert "<script>alert" not in report
  assert hostile_comment.replace("<", "&lt;").replace(">", "&gt;").replace("'", "&#x27;") in report
  for proof in hostile_proofs.values():
    assert proof.replace("<", "&lt;").replace(">", "&gt;").replace("'", "&#x27;") in report


def test_trim_only_reviewer_accepts_a_line_removed_form(tmp_path: Path) -> None:
  outcome, _ = run_variant(tmp_path, "baseline-original-flow")
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  final = base.final_entries_from(proposal, feedback_rich_manifest_dict())
  assert final["entries/render/cache-eviction.md"] == base.canonical_text(TRIMMED_TEXT), (
      "the trim-only reviewer's line removal is the final state")


def test_trim_only_reviewer_writing_new_prose_fails_visibly_after_the_one_re_ask(tmp_path: Path) -> None:
  record = run_variant_expect_failure(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(
          editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_responses=[NEW_PROSE_RESPONSE, NEW_PROSE_RESPONSE]),
      output_dir=tmp_path / "out",
      match="is not a verbatim line of the editor's proposed text")
  assert record["status"] == "failed"
  assert "is not a verbatim line of the editor's proposed text" in record["error"]
  reviewer_calls = [call for call in record["calls"] if call["role"] == "reviewer"]
  assert len(reviewer_calls) == 2 and all(call["validation"]["status"] == "failed" for call in reviewer_calls), (
      "the capability violation is a visible execution failure, both attempts kept")


def test_whole_entry_reviewer_may_rewrite_but_trim_only_may_not(tmp_path: Path) -> None:
  with pytest.raises(ReplayError, match="verbatim line"):
    run_variant(
        tmp_path,
        "baseline-original-flow",
        transport=VariantScriptedTransport(
            editor_response=SELECTOR_EDITOR_RESPONSE,
            reviewer_responses=[WHOLE_REWRITE_RESPONSE, WHOLE_REWRITE_RESPONSE]),
        output_dir=tmp_path / "out-trim")
  outcome, _ = run_variant(
      tmp_path,
      "whole-entry-review",
      transport=VariantScriptedTransport(
          editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=WHOLE_REWRITE_RESPONSE),
      output_dir=tmp_path / "out-whole")
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  final = base.final_entries_from(proposal, feedback_rich_manifest_dict())
  assert final["entries/render/cache-eviction.md"] == base.canonical_text(WHOLE_REWRITE_TEXT), (
      "the same response that fails the trim-only capability is a valid whole-entry rewrite")


def test_trim_only_reviewer_can_restore_the_base_and_reject_new_entries(tmp_path: Path) -> None:
  outcome, _ = run_variant(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=RESTORE_RESPONSE),
      output_dir=tmp_path / "out-restore")
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  assert proposal["reviewed_patch"] == "", "restoring the base leaves no diff"

  drop_new = selector_json(
      [], [reviewer_row(outcome="no_change", paths=[], reason="reversal: the admitted entry fails the entry form")])
  outcome, _ = run_variant(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(editor_response=SELECTOR_NEW_ADMISSION_RESPONSE, reviewer_response=drop_new),
      output_dir=tmp_path / "out-drop")
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  assert proposal["reviewed_patch"] == "", "dropping a selector new leaves the base untouched"
  assert proposal["candidate_results"][0]["outcome"] == "no_change"
  assert "reversal" in proposal["candidate_results"][0]["reason"]


def test_trim_only_reviewer_cannot_delete_or_act_without_a_selector_text_proposal(tmp_path: Path) -> None:
  delete_response = selector_json(
      [{
          "action": "delete",
          "path": "entries/render/cache-eviction.md",
          "reason": "reviewer deleted the entry",
      }], [reviewer_row()])
  record = run_variant_expect_failure(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(
          editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_responses=[delete_response, delete_response]),
      output_dir=tmp_path / "out-delete",
      match="may only confirm an editor delete")
  assert record["status"] == "failed"

  unasked = selector_json(
      [
          {
              "action": "rewrite",
              "path": "entries/render/cache-eviction.md",
              "text": TRIMMED_TEXT,
              "source_refs": ["capture-eviction"],
              "reason": "reviewer-initiated trim",
          }
      ], [reviewer_row()])
  record = run_variant_expect_failure(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(
          editor_response=selector_json([], [selector_row(outcome="no_change", paths=[])]),
          reviewer_responses=[unasked, unasked]),  # editor row: no_change, proof-free
      output_dir=tmp_path / "out-unasked",
      match="never proposed text for")
  assert record["status"] == "failed"


# --- citation availability follows the actual stage input ------------------------


def editor_with_refs(*refs: str, proofs: bool = True) -> str:
  return selector_json(
      [
          {
              "action": "rewrite",
              "path": "entries/render/cache-eviction.md",
              "text": EDITOR_TEXT,
              "source_refs": list(refs),
              "reason": "merged the capture",
          }
      ], [selector_row(proofs=None if not proofs else -1)])


@pytest.mark.parametrize("variant", list(variants.VARIANT_ORDER))
def test_unassigned_document_citation_fails_under_every_variant_initial_and_repair(
    tmp_path: Path, variant: str) -> None:
  bad_response = editor_with_refs("doc-unassigned", proofs=(variant != "combined-proposed-design"))
  record = run_variant_expect_failure(
      tmp_path,
      variant,
      transport=VariantScriptedTransport(editor_response=bad_response, reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / f"out-{variant}",
      match="doc-unassigned")
  assert record["status"] == "failed", variant
  editor_calls = [call for call in record["calls"] if call["role"] == "editor"]
  assert len(editor_calls) == 2, "the same visible-evidence domain is enforced on the repair attempt"
  assert all("doc-unassigned" in json.dumps(call["validation"]) for call in editor_calls)


def test_approved_change_refs_are_citable_only_where_their_content_was_exposed(tmp_path: Path) -> None:
  outcome, _ = run_variant(
      tmp_path,
      "approved-edit-feedback",
      transport=VariantScriptedTransport(
          editor_responses=[editor_with_refs("approved-001"),
                            editor_with_refs("approved-001")],
          reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-selected")
  assert base.run_record(outcome.run_dir)["status"] == "completed", (
      "the selected structured view exposes the approved change, so it is citable")

  record = run_variant_expect_failure(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(
          editor_responses=[editor_with_refs("approved-001"),
                            editor_with_refs("approved-001")],
          reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-raw",
      match="approved-001")
  assert record["status"] == "failed"
  assert "approved-001" in record["error"], (
      "the raw-history view carries no approved revisions, so their refs are not citable")


def test_unselected_comment_ids_are_citable_only_in_the_raw_history_view(tmp_path: Path) -> None:
  outcome, _ = run_variant(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(
          editor_responses=[editor_with_refs("fb-002"), editor_with_refs("fb-002")],
          reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-raw")
  assert base.run_record(
      outcome.run_dir)["status"] == "completed", ("the raw-history view exposes every pool comment, selected or not")

  record = run_variant_expect_failure(
      tmp_path,
      "approved-edit-feedback",
      transport=VariantScriptedTransport(
          editor_responses=[editor_with_refs("fb-002"), editor_with_refs("fb-002")],
          reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-selected",
      match="fb-002")
  assert "fb-002" in record["error"], (
      "a comment the relevance selection did not select was never in the selected view")


def test_reviewer_citations_follow_the_same_visible_view(tmp_path: Path) -> None:
  reviewer_with_refs = selector_json(
      [
          {
              "action": "rewrite",
              "path": "entries/render/cache-eviction.md",
              "text": TRIMMED_TEXT,
              "source_refs": ["fb-002"],
              "reason": "trimmed per the pool comment",
          }
      ], [reviewer_row()])
  outcome, _ = run_variant(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(
          editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=reviewer_with_refs),
      output_dir=tmp_path / "out-raw")
  assert base.run_record(outcome.run_dir)["status"] == "completed"

  record = run_variant_expect_failure(
      tmp_path,
      "approved-edit-feedback",
      transport=VariantScriptedTransport(
          editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_responses=[reviewer_with_refs, reviewer_with_refs]),
      output_dir=tmp_path / "out-selected",
      match="fb-002")
  assert record["status"] == "failed" and "fb-002" in record["error"]


@pytest.mark.parametrize(
    "variant,view_key", [
        ("baseline-original-flow", "feedback_history"), ("approved-edit-feedback", "feedback"),
        ("combined-proposed-design", "feedback")
    ])
def test_repair_requests_carry_the_same_feedback_view_as_the_initial_request(
    tmp_path: Path, variant: str, view_key: str) -> None:
  """The bounded repair re-renders the stage's own view: same feedback key, same citation domain."""
  bad = editor_with_refs("doc-unassigned")  # an unassigned document: invalid in either view
  transport = VariantScriptedTransport(
      editor_responses=[bad, SELECTOR_EDITOR_RESPONSE], reviewer_response=TRIM_ACCEPT_RESPONSE)
  outcome, _ = run_variant(tmp_path, variant, transport=transport, output_dir=tmp_path / f"out-{variant}")
  record = base.run_record(outcome.run_dir)
  assert record["status"] == "completed"
  editor_stage_calls = [call for call in record["calls"] if call["role"] == "editor"]
  assert len(editor_stage_calls) == 2
  initial_payload = payload_of((outcome.run_dir / editor_stage_calls[0]["request_file"]).read_text(encoding="utf-8"))
  repair_text = (outcome.run_dir / editor_stage_calls[1]["request_file"]).read_text(encoding="utf-8")
  repair_payload = payload_of(repair_original_request(repair_text))
  other = "feedback" if view_key == "feedback_history" else "feedback_history"
  for payload in (initial_payload, repair_payload):
    assert view_key in payload and other not in payload
  assert repair_text.startswith("## Original request"), "the repair re-asks over the original authorized evidence"


def test_selected_view_renders_missing_and_empty_approved_sides_honestly(tmp_path: Path) -> None:
  transport = VariantScriptedTransport(editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=TRIM_ACCEPT_RESPONSE)
  run_variant(tmp_path, "approved-edit-feedback", transport=transport)
  payload = payload_of(transport.calls[0]["user"])
  by_event = {row["comment_event"]: row for row in payload["feedback"]}
  assert by_event["fb-004"][
      "approved_change"] is None, "a selected comment without an approved revision stays without one"
  assert "fb-002" not in by_event, "an unselected comment is absent from the selected view"
  change = by_event["fb-003"]["approved_change"]
  assert change["after"] == "", "an approved deletion's empty side travels verbatim, never as invented prose"
  assert change["before"] == base.ENTRY_WITH_INSTANCE
  rendered = json.dumps(payload["feedback"])
  assert "(empty: the approved revision deleted this text)" not in rendered, (
      "the JSON evidence view carries the exact texts; no placeholder prose is injected")
  assert "NO-APPROVED-REVISION-MARKER" in rendered and "UNSELECTED-COMMENT-MARKER" not in rendered


# --- identity, reuse, and dispatch ----------------------------------------------


def test_variant_definition_changes_the_input_identity(tmp_path: Path) -> None:
  from src.core.memory_replay.runner import compute_input_identity

  manifest = base.load_manifest(write_manifest(tmp_path))
  model = {"backend": "fake-clc", "backend_type": "BackendType.CHARLIE_CODE", "model": "fake-model"}
  identities = {
      name: compute_input_identity(manifest, mode="editor-review", model_identity=model, contract=contract)
      for name, contract in variants.VARIANTS.items()
  }
  assert len(set(identities.values())) == len(
      variants.VARIANT_ORDER), ("runs under different variants never reuse each other's bundles")
  assert identities["baseline-original-flow"] == compute_input_identity(
      manifest, mode="editor-review", model_identity=model, contract=variants.resolve_variant("baseline-original-flow"))


def test_standalone_v3_replay_records_no_variant_and_keeps_its_identity(tmp_path: Path) -> None:
  from src.core.memory_replay.exchange import EDITOR_PROMPT_VERSION, REVIEWER_PROMPT_VERSION
  from src.core.memory_replay.identity import input_identity

  manifest_path = base.write_manifest(tmp_path)
  outcome, _ = base.run_replay_with(tmp_path, [base.MERGE_RESPONSE, base.MERGE_RESPONSE], manifest_path=manifest_path)
  record = base.run_record(outcome.run_dir)
  assert "variant" not in record, "standalone v3 bundles keep their exact historical shape"
  manifest = base.load_manifest(manifest_path)
  assert record["input_identity"] == input_identity(
      manifest=manifest,
      mode="editor-review",
      model_identity=record["model"],
      editor_prompt_version=EDITOR_PROMPT_VERSION,
      reviewer_prompt_version=REVIEWER_PROMPT_VERSION)


def test_registry_resolves_names_and_rejects_unknown_ones() -> None:
  assert set(variants.VARIANTS) == set(variants.VARIANT_ORDER)
  with pytest.raises(ReplayError, match="unknown experiment variant"):
    variants.resolve_variant("no-such-variant")


# --- experiment end to end --------------------------------------------------------

COMPLETE_RESPONSES = dict(editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=TRIM_ACCEPT_RESPONSE)


def experiment_cfg(tmp_path: Path) -> CharlieBotConfig:
  return replay_cfg(tmp_path)


def test_experiment_runs_the_matrix_preserves_a_failing_arm_and_reuses_completed_arms(tmp_path: Path) -> None:
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml"), write_manifest(tmp_path, name="case-beta.yaml")]
  output_dir = tmp_path / "exp"
  # The combined editor of the first case dies on its single call (transport failures are never
  # retried): one failing arm, every other arm completes.
  transport = VariantScriptedTransport(die_on_call=3, **COMPLETE_RESPONSES)
  result = run_experiment(
      ExperimentOptions(
          manifests=manifests,
          output_dir=output_dir,
          backend="fake-clc",
          variants=["baseline-original-flow", "combined-proposed-design"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: transport)
  assert result.failed_arms == ["case-alpha/combined-proposed-design"]

  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  assert summary["schema"] == "memory-curation-variant-experiment/2"
  assert [case["case"] for case in summary["cases"]] == ["case-alpha", "case-beta"]
  statuses = {
      (arm["case"], arm["variant"]): (arm["run"]["status"], arm["comparison"]["status"]) for case in summary["cases"]
      for arm in case["arms"]
  }
  assert statuses[("case-alpha", "baseline-original-flow")] == ("completed", "established")
  assert statuses[("case-alpha", "combined-proposed-design")] == ("failed", "established"), (
      "the arm whose editor transport died is the one failing arm")
  assert statuses[("case-beta", "baseline-original-flow")] == ("completed", "established")
  assert statuses[("case-beta", "combined-proposed-design")] == ("completed", "established")

  failed_arm = next(
      arm for case in summary["cases"] for arm in case["arms"]
      if (arm["case"], arm["variant"]) == ("case-alpha", "combined-proposed-design"))
  assert failed_arm["comparison"]["editor_only"]["status"] == "failed", (
      "a failed arm is never substituted with no_change output")
  assert failed_arm["comparison"]["post_review"]["status"] == "failed"
  assert failed_arm["run"]["usage"]["editor"]["calls"] == 1, "the failed attempt's usage stays recorded"

  for case in summary["cases"]:
    for arm in case["arms"]:
      assert arm["denominators"] == {
          "themes": 1,
          "theme_names": ["eviction"],
          "input_candidates": 1,
          "candidates_per_theme": {
              "eviction": 1
          },
      }, "case, theme, and candidate denominators stay fixed for every variant"

  # The readable report links exact replay and comparison artifacts: every href resolves.
  import re
  hrefs = re.findall(r'href="([^"]+)"', (output_dir / "report.html").read_text(encoding="utf-8"))
  assert hrefs, "the report links the replay and comparison artifacts"
  for href in hrefs:
    assert (output_dir / href).is_file(), f"report.html link {href} does not resolve under the output root"

  # Repeat the experiment: completed arms reuse their bundles, the failed arm is preserved and
  # not rerun, and no model call is made at all.
  fresh = VariantScriptedTransport(**COMPLETE_RESPONSES)
  result_again = run_experiment(
      ExperimentOptions(
          manifests=manifests,
          output_dir=output_dir,
          backend="fake-clc",
          variants=["baseline-original-flow", "combined-proposed-design"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: fresh)
  assert fresh.calls == [], "repeating a settled experiment makes no model calls"
  assert result_again.failed_arms == ["case-alpha/combined-proposed-design"], "failures stay recorded"
  summary_again = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  failed_again = next(
      arm for case in summary_again["cases"] for arm in case["arms"]
      if (arm["case"], arm["variant"]) == ("case-alpha", "combined-proposed-design"))
  assert failed_again["run"]["status"] == "failed"
  assert failed_again["run"]["preserved_failed_attempts"], "the preserved failed bundle stays visible"


def test_experiment_derives_the_editor_only_control_from_the_shared_response(tmp_path: Path) -> None:
  manifest = write_manifest(tmp_path, name="case-alpha.yaml")
  output_dir = tmp_path / "exp"
  run_experiment(
      ExperimentOptions(
          manifests=[manifest], output_dir=output_dir, backend="fake-clc", variants=["baseline-original-flow"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: VariantScriptedTransport(**COMPLETE_RESPONSES))
  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  arm = summary["cases"][0]["arms"][0]
  run_dir = output_dir / "cases" / "case-alpha" / "runs" / "baseline-original-flow" / "runs"
  run_dir = next(iter(run_dir.iterdir()))
  raw_editor_response = (run_dir / "raw" / "editor-eviction.attempt-1.response.txt").read_text(encoding="utf-8")
  comparison = json.loads(
      (output_dir / "cases" / "case-alpha" / "comparisons" / "baseline-original-flow" /
       "comparison.json").read_text(encoding="utf-8"))
  assert comparison["model_calls_made"] == 0
  shared = comparison["provenance"]["shared_editor_response"]["themes"]["eviction"]
  assert shared["editor_response_sha256"] == sha256_hex(raw_editor_response.encode("utf-8")), (
      "the control derives from the exact editor response the reviewer consumed")
  provenance = arm["editor_provenance"]["themes"]["eviction"]
  assert provenance["chosen_response"]["response_sha256"] == shared["editor_response_sha256"]
  editor_only = comparison["arms"]["editor-only"]
  assert editor_only["status"] == "established"
  assert editor_only["reviewed_patch"] != comparison["arms"]["post-review"]["reviewed_patch"], (
      "the reviewer's trim must be visible as a different final state from the editor's own proposal")
  final = base.apply_unified_patch(
      {"entries/render/cache-eviction.md": base.canonical_text(base.ENTRY_WITH_INSTANCE)},
      editor_only["reviewed_patch"])
  assert final["entries/render/cache-eviction.md"] == base.canonical_text(EDITOR_TEXT), (
      "the editor-only arm is the recorded editor response's own final state")


def test_editor_call_provenance_reports_actual_calls_and_labels_identical_content(tmp_path: Path) -> None:
  """Two real editor calls that returned identical bytes: two calls, one identical content."""
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml")]
  output_dir = tmp_path / "exp"
  transports: list[VariantScriptedTransport] = []

  def factory() -> VariantScriptedTransport:
    transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
    transports.append(transport)
    return transport

  run_experiment(
      ExperimentOptions(
          manifests=manifests,
          output_dir=output_dir,
          backend="fake-clc",
          variants=["baseline-original-flow", "rationale-hidden-review"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=factory)
  actual_editor_calls = [call for transport in transports for call in editor_calls(transport)]
  assert len(actual_editor_calls) == 2, "each variant's editor is really called; equal bytes save no call"

  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  section = summary["cases"][0]["editor_calls"]
  assert section["actual_editor_calls"] == 2
  assert section["per_variant"]["baseline-original-flow"]["actual_editor_calls"] == 1
  assert section["per_variant"]["rationale-hidden-review"]["actual_editor_calls"] == 1
  assert section["identical_chosen_content"] == {
      "eviction": [["baseline-original-flow", "rationale-hidden-review"]]
  }, ("byte-identical recorded content is labeled identical content, not shared sampling")
  assert section["variants_without_chosen_editor_response"] == []
  assert "identical content" in section["note"] and "shared sampling" in section["note"]
  report = (output_dir / "report.html").read_text(encoding="utf-8")
  assert "identical chosen editor content" in report
  assert "shared byte-identical responses" not in report, "the misleading draw label is gone"

  arms = {arm["variant"]: arm for arm in summary["cases"][0]["arms"]}
  for name, transport in (("baseline-original-flow", transports[0]), ("rationale-hidden-review", transports[1])):
    arm = arms[name]
    provenance = arm["editor_provenance"]
    assert provenance["run_record"] == arm["run"]["record"]
    theme = provenance["themes"]["eviction"]
    assert theme["actual_calls"] == 1 and len(theme["attempts"]) == 1
    chosen = theme["chosen_response"]
    assert chosen["call"] == "editor-eviction.attempt-1" and chosen["attempt"] == 1
    response_path = output_dir / arm["run"]["dir"] / chosen["response_file"]
    assert chosen["response_sha256"] == sha256_hex(response_path.read_bytes())
    assert arm["run"]["usage"]["editor"]["calls"] == 1, "each arm keeps its own usage record"
    assert arm["run"]["usage"]["editor"]["output_tokens"] == 42
    comparison = json.loads(
        (output_dir / "cases" / "case-alpha" / "comparisons" / name / "comparison.json").read_text(encoding="utf-8"))
    shared = comparison["provenance"]["shared_editor_response"]["themes"]["eviction"]
    assert shared["editor_response_sha256"] == chosen["response_sha256"], (
        "each paired no-review control derives from that arm's actual recorded source")


def test_editor_call_provenance_compares_content_per_theme_across_variants(tmp_path: Path) -> None:
  """A multi-theme case: identical on one theme, different on another — equality is per theme."""
  manifest_path = write_manifest(tmp_path, name="case-alpha.yaml", data=two_theme_manifest_dict())
  manifests = [manifest_path]
  output_dir = tmp_path / "exp"
  # Manifest themes load in sorted name order: alerts first, eviction second. The alerts responses
  # differ across the variants; the eviction response is byte-identical for both.
  theme_names = [theme.name for theme in base.load_manifest(manifest_path).themes]
  assert theme_names == ["alerts", "eviction"], theme_names
  first = VariantScriptedTransport(
      editor_responses=[ALERTS_EDITOR_RESPONSE_A, SELECTOR_EDITOR_RESPONSE], reviewer_response=TRIM_ACCEPT_RESPONSE)
  second = VariantScriptedTransport(
      editor_responses=[ALERTS_EDITOR_RESPONSE_B, SELECTOR_EDITOR_RESPONSE], reviewer_response=TRIM_ACCEPT_RESPONSE)
  run_experiment(
      ExperimentOptions(
          manifests=manifests,
          output_dir=output_dir,
          backend="fake-clc",
          variants=["baseline-original-flow", "rationale-hidden-review"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: first if not first.calls else second)
  assert len(editor_calls(first)) == 2 and len(editor_calls(second)) == 2, (
      "four real editor calls across the two variants x two themes; equal theme bytes save none")

  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  section = summary["cases"][0]["editor_calls"]
  assert section["actual_editor_calls"] == 4
  assert section["per_variant"]["baseline-original-flow"]["actual_editor_calls"] == 2
  assert section["per_variant"]["rationale-hidden-review"]["actual_editor_calls"] == 2
  assert section["identical_chosen_content"] == {
      "eviction": [["baseline-original-flow", "rationale-hidden-review"]]
  }, ("equality is reported per theme; the differing theme is not grouped")
  for arm in summary["cases"][0]["arms"]:
    themes = arm["editor_provenance"]["themes"]
    assert set(themes) == {"eviction", "alerts"}, "every theme is accounted for"
    for theme in themes.values():
      assert theme["actual_calls"] == 1 and theme["chosen_response"] is not None


def test_editor_provenance_survives_a_failed_reviewer(tmp_path: Path) -> None:
  """The editor succeeded and the reviewer failed: the editor's provenance stays visible."""
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml")]
  output_dir = tmp_path / "exp"
  transport = VariantScriptedTransport(
      editor_response=SELECTOR_EDITOR_RESPONSE, reviewer_responses=[NEW_PROSE_RESPONSE, NEW_PROSE_RESPONSE])
  result = run_experiment(
      ExperimentOptions(
          manifests=manifests, output_dir=output_dir, backend="fake-clc", variants=["baseline-original-flow"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: transport)
  assert result.failed_arms == ["case-alpha/baseline-original-flow"]
  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  arm = summary["cases"][0]["arms"][0]
  assert arm["run"]["status"] == "failed"
  chosen = arm["editor_provenance"]["themes"]["eviction"]["chosen_response"]
  assert chosen is not None and chosen["attempt"] == 1, "the editor's established response stays reported"
  run_dir = output_dir / arm["run"]["dir"]
  assert chosen["response_sha256"] == sha256_hex(
      (run_dir / chosen["response_file"]).read_bytes()), "the digest matches the recorded bytes"
  assert arm["editor_provenance"]["actual_editor_calls"] == 1
  assert arm["comparison"]["editor_only"]["status"] == "established", (
      "the editor-only control is still derived from the editor response the failed reviewer consumed")
  assert arm["comparison"]["post_review"]["status"] == "failed"


def test_experiment_rejects_unknown_variants_and_duplicate_case_ids(tmp_path: Path) -> None:
  manifest = write_manifest(tmp_path, name="case-alpha.yaml")
  with pytest.raises(ReplayError, match="unknown experiment variant"):
    run_experiment(
        ExperimentOptions(
            manifests=[manifest], output_dir=tmp_path / "e1", backend="fake-clc", variants=["no-such-variant"]),
        cfg=experiment_cfg(tmp_path))
  duplicate = write_manifest(tmp_path, name="case-alpha.yaml", data=base.base_manifest_dict())
  with pytest.raises(ReplayError, match="share the case id"):
    run_experiment(
        ExperimentOptions(manifests=[manifest, duplicate], output_dir=tmp_path / "e2", backend="fake-clc"),
        cfg=experiment_cfg(tmp_path))


# --- the experiment boundary: existing evidence is validated before any replay runs -------------


def run_completed_experiment(tmp_path: Path, manifests: list[Path], output_dir: Path) -> None:
  run_experiment(
      ExperimentOptions(
          manifests=manifests, output_dir=output_dir, backend="fake-clc", variants=["baseline-original-flow"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: VariantScriptedTransport(**COMPLETE_RESPONSES))


def variant_runs_root(output_dir: Path, case_id: str = "case-alpha", variant: str = "baseline-original-flow") -> Path:
  return output_dir / "cases" / case_id / "runs" / variant / "runs"


def test_corrupted_input_identity_blocks_the_arm_and_preserves_the_original_bytes(tmp_path: Path) -> None:
  """A corrupted input identity: the repeat makes no call, keeps the bytes, reports the block."""
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml"), write_manifest(tmp_path, name="case-beta.yaml")]
  output_dir = tmp_path / "exp"
  run_completed_experiment(tmp_path, manifests, output_dir)

  run_dir = next(iter(variant_runs_root(output_dir).iterdir()))
  record_path = run_dir / "run.json"
  record = json.loads(record_path.read_text(encoding="utf-8"))
  record["input_identity"] = "corrupted-recorded-identity"
  record["probe_marker"] = "preserve-this-original-corruption-evidence"
  record_path.write_text(json.dumps(record) + "\n", encoding="utf-8")
  before = snapshot_files(run_dir)
  assert b"preserve-this-original-corruption-evidence" in before[Path("run.json")]

  transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
  result = run_experiment(
      ExperimentOptions(
          manifests=manifests, output_dir=output_dir, backend="fake-clc", variants=["baseline-original-flow"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: transport)
  assert transport.calls == [], "no model call may follow an uninterpretable run record"
  assert snapshot_files(run_dir) == before, "the corrupted evidence is preserved byte-for-byte, not repaired"

  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  arms = {(arm["case"], arm["variant"]): arm for case in summary["cases"] for arm in case["arms"]}
  blocked = arms[("case-alpha", "baseline-original-flow")]
  assert blocked["run"]["status"] == "blocked"
  assert len(blocked["run"]["blocked_evidence"]) == 1
  assert "identity/path disagreement" in blocked["run"]["blocked_evidence"][0]["reason"]
  assert "preserved byte-for-byte" in blocked["run"]["error"]
  assert blocked["run"]["record"] == (f"cases/case-alpha/runs/baseline-original-flow/runs/{run_dir.name}/run.json")
  assert blocked["comparison"]["status"] == "blocked"
  assert result.failed_arms == ["case-alpha/baseline-original-flow"]

  # The independent case continues and reuses its completed bundle without calls.
  continuing = arms[("case-beta", "baseline-original-flow")]
  assert continuing["run"]["status"] == "completed" and continuing["run"]["reused"] is True
  assert continuing["run"]["blocked_evidence"] == []
  assert continuing["denominators"]["input_candidates"] == 1


def test_missing_run_record_with_partial_evidence_blocks_the_arm(tmp_path: Path) -> None:
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml")]
  output_dir = tmp_path / "exp"
  run_completed_experiment(tmp_path, manifests, output_dir)
  run_dir = next(iter(variant_runs_root(output_dir).iterdir()))
  (run_dir / "run.json").unlink()
  assert (run_dir / "raw").is_dir(), "the orphaned directory still holds recorded attempt evidence"
  before = snapshot_files(run_dir)

  transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
  run_experiment(
      ExperimentOptions(
          manifests=manifests, output_dir=output_dir, backend="fake-clc", variants=["baseline-original-flow"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: transport)
  assert transport.calls == []
  assert snapshot_files(run_dir) == before
  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  arm = summary["cases"][0]["arms"][0]
  assert arm["run"]["status"] == "blocked"
  assert "run record run.json is missing" in arm["run"]["blocked_evidence"][0]["reason"]


def test_unreadable_run_record_blocks_the_arm(tmp_path: Path) -> None:
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml")]
  output_dir = tmp_path / "exp"
  run_completed_experiment(tmp_path, manifests, output_dir)
  run_dir = next(iter(variant_runs_root(output_dir).iterdir()))
  (run_dir / "run.json").write_text("{not json", encoding="utf-8")
  before = snapshot_files(run_dir)

  transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
  run_experiment(
      ExperimentOptions(
          manifests=manifests, output_dir=output_dir, backend="fake-clc", variants=["baseline-original-flow"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: transport)
  assert transport.calls == []
  assert snapshot_files(run_dir) == before
  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  arm = summary["cases"][0]["arms"][0]
  assert arm["run"]["status"] == "blocked"
  assert "unreadable" in arm["run"]["blocked_evidence"][0]["reason"]


def test_different_legitimate_identities_coexist_in_distinct_run_directories(tmp_path: Path) -> None:
  """Changed frozen inputs are a new identity, not corruption: both bundles coexist untouched."""
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml")]
  output_dir = tmp_path / "exp"
  run_completed_experiment(tmp_path, manifests, output_dir)
  first_run_dir = next(iter(variant_runs_root(output_dir).iterdir()))
  before = snapshot_files(first_run_dir)

  changed = feedback_rich_manifest_dict()
  changed["base_commit"] = "mem-base-0002"
  new_manifests = [write_manifest(tmp_path, data=changed, name="case-alpha.yaml")]
  transport = VariantScriptedTransport(**COMPLETE_RESPONSES)
  result = run_experiment(
      ExperimentOptions(
          manifests=new_manifests, output_dir=output_dir, backend="fake-clc", variants=["baseline-original-flow"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: transport)
  assert result.failed_arms == []
  assert transport.calls, "the changed inputs legitimately draw fresh; coexistence is not a rerun of the old"
  assert snapshot_files(first_run_dir) == before
  assert len(list(variant_runs_root(output_dir).iterdir())) == 2
  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  arm = summary["cases"][0]["arms"][0]
  assert arm["run"]["status"] == "completed" and arm["run"]["reused"] is False
  assert arm["run"]["blocked_evidence"] == []


# --- tamper detection and legacy contracts ---------------------------------------


def completed_variant_run(tmp_path: Path, variant: str = "baseline-original-flow", name: str = "out") -> Path:
  outcome, _ = run_variant(
      tmp_path,
      variant,
      output_dir=tmp_path / name,
      manifest_path=write_manifest(tmp_path, name=f"{variant}-{name}.yaml"))
  return outcome.run_dir


def test_comparison_rejects_a_tampered_variant_definition(tmp_path: Path) -> None:
  run_dir = completed_variant_run(tmp_path)
  record_path = run_dir / "run.json"
  record = json.loads(record_path.read_text(encoding="utf-8"))
  record["variant"]["feedback_view"] = "selected-structured"
  record_path.write_text(json.dumps(record), encoding="utf-8")
  with pytest.raises(ReplayError, match="no longer matches"):
    run_comparison(CompareOptions(run_dir=run_dir, output_dir=tmp_path / "cmp"), cfg=replay_cfg(tmp_path))


def test_comparison_rejects_unknown_drifted_or_misversioned_variants(tmp_path: Path) -> None:
  from src.core.memory_replay.compare import _contract_for

  run_dir = completed_variant_run(tmp_path)
  record = base.run_record(run_dir)
  unknown = dict(record)
  unknown["variant"] = dict(record["variant"], name="no-such-variant")
  with pytest.raises(ReplayError, match="does not define"):
    _contract_for(unknown)
  drifted = dict(record)
  drifted["variant"] = dict(record["variant"], version=record["variant"]["version"] + 1)
  with pytest.raises(ReplayError, match="experimental definition version"):
    _contract_for(drifted)
  misversioned = dict(record)
  misversioned["prompt_versions"] = {
      "editor": "memory-experiment-editor-selector-raw-history-v9",
      "reviewer": record["prompt_versions"]["reviewer"]
  }
  with pytest.raises(ReplayError, match="prompt versions"):
    _contract_for(misversioned)


def test_legacy_v1_variant_records_are_rejected_under_their_version_and_kept_interpretable_nowhere(
    tmp_path: Path) -> None:
  """A recorded v1 matrix is neither reused nor relabeled: its version is named on rejection."""
  from src.core.memory_replay.compare import _contract_for

  run_dir = completed_variant_run(tmp_path)
  record = base.run_record(run_dir)
  legacy = dict(record)
  legacy["variant"] = {
      "name": record["variant"]["name"],
      "version": 1,
      "editor_stage": "baseline-selector",
      "reviewer_stage": "baseline-trim",
      "feedback_view": "raw-history",
      "rationale_visibility": "visible",
      "reviewer_capability": "trim-only",
  }
  legacy["prompt_versions"] = {
      "editor": "memory-experiment-editor-selector-raw-history-v1",
      "reviewer": "memory-experiment-reviewer-trim-raw-history-visible-v1",
  }
  with pytest.raises(ReplayError, match="experimental definition version 1"):
    _contract_for(legacy)
  stripped = dict(record)
  stripped["variant"] = {key: value for key, value in record["variant"].items() if key != "entry_scope"}
  with pytest.raises(ReplayError, match="missing entry_scope"):
    _contract_for(stripped)
  variantless = {key: value for key, value in record.items() if key != "variant"}
  with pytest.raises(ReplayError, match="unsupported replay prompt version"):
    _contract_for(variantless)


def test_comparison_detects_tampered_rationale_response_and_feedback_provenance(tmp_path: Path) -> None:
  run_dir = completed_variant_run(tmp_path, name="out-tamper-1")
  response_path = run_dir / "raw" / "editor-eviction.attempt-1.response.txt"
  response = response_path.read_text(encoding="utf-8")
  response_path.write_text(response.replace(PROOFS["action"], "TAMPERED-ACTION-PROOF"), encoding="utf-8")
  with pytest.raises(ReplayError, match="no longer matches the hash recorded at run time"):
    run_comparison(CompareOptions(run_dir=run_dir, output_dir=tmp_path / "cmp1"), cfg=replay_cfg(tmp_path))

  run_dir = completed_variant_run(tmp_path, name="out-tamper-2")
  request_path = run_dir / "raw" / "reviewer-eviction.attempt-1.request.txt"
  request = request_path.read_text(encoding="utf-8")
  request_path.write_text(request.replace("editor_proposals", "editor_suggestions"), encoding="utf-8")
  with pytest.raises(ReplayError, match="no longer matches the hash recorded at run time"):
    run_comparison(CompareOptions(run_dir=run_dir, output_dir=tmp_path / "cmp2"), cfg=replay_cfg(tmp_path))

  run_dir = completed_variant_run(tmp_path, name="out-tamper-3")
  record_path = run_dir / "run.json"
  record = json.loads(record_path.read_text(encoding="utf-8"))
  proposal_path = run_dir / "proposal.json"
  proposal = json.loads(proposal_path.read_text(encoding="utf-8"))
  # bundle_integrity covers proposal.json, so strip the recorded hash the way an older record
  # would look, then tamper the feedback provenance the stages saw.
  record["bundle_integrity"].pop("proposal.json")
  record_path.write_text(json.dumps(record), encoding="utf-8")
  proposal["feedback_refs"] = [{"comment_event": "fb-002", "approved_change_ref": None}]
  proposal_path.write_text(json.dumps(proposal), encoding="utf-8")
  with pytest.raises(ReplayError, match="feedback_refs do not match"):
    run_comparison(CompareOptions(run_dir=run_dir, output_dir=tmp_path / "cmp3"), cfg=replay_cfg(tmp_path))


def test_raw_history_proposal_names_the_pool_without_unexposed_approved_refs(tmp_path: Path) -> None:
  outcome, _ = run_variant(tmp_path, "baseline-original-flow")
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  assert proposal["feedback_refs"] == [
      {
          "comment_event": event,
          "approved_change_ref": None
      } for event in ("fb-001", "fb-002", "fb-003", "fb-004")
  ], ("the raw-history stages saw every comment and no approved revisions")


# --- CLI -------------------------------------------------------------------------


def _write_cli_profile_config() -> None:
  base._write_cli_profile_config()


class _SharedStubTransportClass:
  """Every from_config returns one shared scripted transport, so call counts are observable."""

  shared: VariantScriptedTransport | None = None

  def __init__(self) -> None:
    assert _SharedStubTransportClass.shared is not None

  def __getattr__(self, name: str):
    return getattr(_SharedStubTransportClass.shared, name)

  @classmethod
  def from_config(cls, option, cfg=None):
    return cls.shared


def test_cli_experiment_end_to_end_with_a_failing_arm_and_reuse(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  import tests.conftest as conftest_module
  from src.cli import memory as memory_cli
  from src.core.memory_replay import runner as runner_module

  _write_cli_profile_config()
  conftest_module.reset_config_caches()
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml"), write_manifest(tmp_path, name="case-beta.yaml")]
  output_dir = tmp_path / "cli-out"
  _SharedStubTransportClass.shared = VariantScriptedTransport(die_on_call=3, **COMPLETE_RESPONSES)
  monkeypatch.setattr(runner_module, "OpenAICompatibleTransport", _SharedStubTransportClass)
  monkeypatch.setattr(
      sys, "argv", [
          "charliebot memory", "experiment", "--input",
          str(manifests[0]),
          str(manifests[1]), "--output-dir",
          str(output_dir), "--backend", "fake-clc", "--variants", "baseline-original-flow", "combined-proposed-design"
      ])
  with pytest.raises(SystemExit) as exc_info:
    memory_cli.main()
  assert exc_info.value.code == 1, "a failing arm is an execution failure; the summary is still written"
  out = capsys.readouterr().out
  assert "arm case-alpha/baseline-original-flow: run completed" in out
  assert "arm case-alpha/combined-proposed-design: run failed" in out
  assert "failed arms (recorded, not rerun under this output root): case-alpha/combined-proposed-design" in out
  assert (output_dir / "experiment.json").is_file() and (output_dir / "report.html").is_file()
  first_run_calls = _SharedStubTransportClass.shared.calls
  assert len(first_run_calls) == 7, (
      "2 cases x 2 variants x 2 stages minus the reviewer call the dead editor arm never reached")

  # Repeat: no new model calls; completed arms reuse, the failed arm stays preserved.
  _SharedStubTransportClass.shared = VariantScriptedTransport(**COMPLETE_RESPONSES)
  monkeypatch.setattr(
      sys, "argv", [
          "charliebot memory", "experiment", "--input",
          str(manifests[0]),
          str(manifests[1]), "--output-dir",
          str(output_dir), "--backend", "fake-clc", "--variants", "baseline-original-flow", "combined-proposed-design"
      ])
  with pytest.raises(SystemExit) as exc_info:
    memory_cli.main()
  assert exc_info.value.code == 1
  assert _SharedStubTransportClass.shared.calls == [], "the repeated invocation makes no model calls"
  assert "arm case-alpha/combined-proposed-design: run failed" in capsys.readouterr().out


def test_cli_experiment_reports_a_blocked_arm_with_its_preserved_evidence(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  import tests.conftest as conftest_module
  from src.cli import memory as memory_cli
  from src.core.memory_replay import runner as runner_module

  _write_cli_profile_config()
  conftest_module.reset_config_caches()
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml")]
  output_dir = tmp_path / "cli-out"
  argv = [
      "charliebot memory", "experiment", "--input",
      str(manifests[0]), "--output-dir",
      str(output_dir), "--backend", "fake-clc", "--variants", "baseline-original-flow"
  ]
  _SharedStubTransportClass.shared = VariantScriptedTransport(**COMPLETE_RESPONSES)
  monkeypatch.setattr(runner_module, "OpenAICompatibleTransport", _SharedStubTransportClass)
  monkeypatch.setattr(sys, "argv", argv)
  memory_cli.main()  # a completed experiment exits 0 by returning
  capsys.readouterr()

  run_dir = next(iter((output_dir / "cases" / "case-alpha" / "runs" / "baseline-original-flow" / "runs").iterdir()))
  record_path = run_dir / "run.json"
  record = json.loads(record_path.read_text(encoding="utf-8"))
  record["input_identity"] = "corrupted-recorded-identity"
  record_path.write_text(json.dumps(record) + "\n", encoding="utf-8")

  _SharedStubTransportClass.shared = VariantScriptedTransport(**COMPLETE_RESPONSES)
  monkeypatch.setattr(sys, "argv", argv)
  with pytest.raises(SystemExit) as exc_info:
    memory_cli.main()
  assert exc_info.value.code == 1
  out = capsys.readouterr().out
  assert "arm case-alpha/baseline-original-flow: run blocked" in out
  assert "comparison blocked (existing evidence preserved)" in out
  assert "failed arms (recorded, not rerun under this output root): case-alpha/baseline-original-flow" in out
  assert _SharedStubTransportClass.shared.calls == [], "the blocked repeat makes no model calls"


def test_cli_experiment_reports_input_errors_without_a_summary(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  import tests.conftest as conftest_module
  from src.cli import memory as memory_cli

  _write_cli_profile_config()
  conftest_module.reset_config_caches()
  monkeypatch.setattr(
      sys, "argv", [
          "charliebot memory", "experiment", "--input",
          str(tmp_path / "missing.yaml"), "--output-dir",
          str(tmp_path / "out"), "--backend", "fake-clc"
      ])
  with pytest.raises(SystemExit) as exc_info:
    memory_cli.main()
  assert exc_info.value.code == 1
  err = capsys.readouterr().err
  assert err.startswith("error: ") and "missing.yaml" in err
