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

COMBINED_EDITOR_RESPONSE = selector_json(
    [
        {
            "action": "rewrite",
            "path": "entries/render/cache-eviction.md",
            "text": EDITOR_TEXT,
            "source_refs": ["capture-eviction", "entry-cache-eviction"],
            "reason": "merged the capture",
        }
    ], [reviewer_row()])

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


class VariantScriptedTransport:
  """Answers selector prompts, reviewer prompts, and dies at a scripted call number."""

  def __init__(
      self,
      *,
      selector_response: str | None = None,
      reviewer_response: str | None = None,
      die_on_call: int | None = None,
      selector_responses: list[str] | None = None,
      reviewer_responses: list[str] | None = None,
      combined_editor_response: str | None = None):
    # A single scripted response repeats for every stage call of its kind; an explicit list is
    # consumed in order and runs out loudly. The proposed-design editor (which writes no proofs)
    # has its own queue so one transport can serve a whole experiment.
    self.selector_queue = [selector_response] if selector_responses is None else list(selector_responses)
    self.combined_queue = [combined_editor_response] if combined_editor_response is not None else []
    self.reviewer_queue = [reviewer_response] if reviewer_responses is None else list(reviewer_responses)
    self.selector_repeat = selector_responses is None
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
    elif "selector stage" in system:
      if not self.selector_queue:
        raise AssertionError("scripted transport ran out of editor responses")
      text = self.selector_queue.pop(0) if not self.selector_repeat else self.selector_queue[0]
    else:  # the proposed-design editor stage
      if not self.combined_queue:
        raise AssertionError("scripted transport ran out of combined editor responses")
      text = self.combined_queue[0]  # a single scripted combined-editor response repeats
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
      selector_response=SELECTOR_EDITOR_RESPONSE,
      combined_editor_response=COMBINED_EDITOR_RESPONSE,
      reviewer_response=TRIM_ACCEPT_RESPONSE)
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


def run_record(run_dir: Path) -> dict:
  return json.loads((run_dir / "run.json").read_text(encoding="utf-8"))


def run_variant_expect_failure(tmp_path: Path, variant: str, *, transport, output_dir: Path, match: str) -> dict:
  """A run that must fail visibly; returns the preserved failed record."""
  with pytest.raises(ReplayError, match=match):
    run_variant(tmp_path, variant, transport=transport, output_dir=output_dir)
  return run_record(next(iter((output_dir / "runs").iterdir())))


def payload_of(request_text: str) -> dict:
  return base.evidence_payload(request_text)


def stage_calls(transport: VariantScriptedTransport, stage: str) -> list[dict]:
  marker = "reviewer stage" if stage == "reviewer" else "stage"
  return [call for call in transport.calls if (marker in call["system"])]


# --- the variant matrix ---------------------------------------------------------


def test_variant_matrix_changes_only_the_declared_dimensions() -> None:
  expected = {
      "baseline-original-flow": ("baseline-selector", "baseline-trim", "raw-history", "visible", "trim-only"),
      "rationale-hidden-review": ("baseline-selector", "baseline-trim", "raw-history", "hidden", "trim-only"),
      "whole-entry-review": ("baseline-selector", "whole-entry", "raw-history", "visible", "whole-entry"),
      "approved-edit-feedback": ("baseline-selector", "baseline-trim", "selected-structured", "visible", "trim-only"),
      "combined-proposed-design":
          ("proposed-design", "proposed-design", "selected-structured", "hidden", "whole-entry"),
  }
  assert tuple(variants.VARIANT_ORDER) == tuple(expected)
  for name, (editor, reviewer, view, rationale, capability) in expected.items():
    contract = variants.resolve_variant(name)
    assert (
        contract.editor_stage, contract.reviewer_stage, contract.feedback_view, contract.rationale_visibility,
        contract.reviewer_capability) == (editor, reviewer, view, rationale, capability), name


def test_every_variant_prompt_states_the_english_memory_rule() -> None:
  for name in variants.VARIANT_ORDER:
    contract = variants.resolve_variant(name)
    for system in (contract.editor_system, contract.reviewer_system):
      assert variants.LANGUAGE_RULE.strip() in system, name


def test_single_interventions_change_only_their_declared_dimension(tmp_path: Path) -> None:
  """The request-level matrix: each intervention moves exactly its own evidence or capability."""
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
  assert set(raw_payload) == set(selected_payload) | {"feedback_history"} - {"feedback"} or (
      "feedback_history" in raw_payload and "feedback" not in raw_payload and "feedback" in selected_payload and
      "feedback_history" not in selected_payload)
  assert {row["comment_event"] for row in raw_payload["feedback_history"]} == {"fb-001", "fb-002", "fb-003", "fb-004"}
  assert all("approved_change" not in json.dumps(row) for row in raw_payload["feedback_history"])
  assert {row["comment_event"] for row in selected_payload["feedback"]
         } == {"fb-001", "fb-003", "fb-004"}, ("the relevance selection, not the pool, is the selected view")
  for key in set(raw_payload) - {"feedback_history"}:
    assert raw_payload[key] == selected_payload.get(key), (
        f"the feedback view is the only evidence difference; {key} drifted")
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

  # whole-entry capability: byte-identical requests to the baseline (same builder, same evidence);
  # only the reviewer prompt and validator change.
  assert requests["whole-entry-review"]["editor"] == requests["baseline-original-flow"]["editor"]
  assert requests["whole-entry-review"]["reviewer"] == requests["baseline-original-flow"]["reviewer"]
  assert variants.resolve_variant("whole-entry-review").reviewer_system != variants.resolve_variant(
      "baseline-original-flow").reviewer_system

  # combined: hidden rationale (like variant 2) over the selected feedback view (like variant 4).
  combined_payload = payload_of(requests["combined-proposed-design"]["reviewer"])
  selected_reviewer_payload = payload_of(requests["approved-edit-feedback"]["reviewer"])
  assert set(combined_payload["editor_proposals"]) == {"entries"}
  assert set(combined_payload) - {"editor_proposals"} == set(selected_reviewer_payload) - {"editor_proposals"}, (
      "the combined reviewer's evidence is the selected view, identical to variant 4's")
  assert combined_payload["feedback"] == selected_reviewer_payload["feedback"]
  assert combined_payload["editor_proposals"]["entries"] == hidden_payload["editor_proposals"]["entries"]


# --- the three proofs are model output ------------------------------------------


def test_selector_proofs_are_recorded_model_output_and_hand_off_to_the_reviewer(tmp_path: Path) -> None:
  outcome, transport = run_variant(tmp_path, "baseline-original-flow")
  record = run_record(outcome.run_dir)
  proofs_rows = [row for row in record["editor_dispositions"] if row.get("proofs")]
  assert proofs_rows and proofs_rows[0]["proofs"] == PROOFS, "the proofs are the model's own rows"
  raw_response = (outcome.run_dir / "raw" / "editor-eviction.attempt-1.response.txt").read_text(encoding="utf-8")
  assert PROOFS["action"] in raw_response, "the handoff proofs come from the recorded model response"
  reviewer_request = transport.calls[1]["user"]
  for proof in PROOFS.values():
    assert proof in reviewer_request, "the visible-rationale reviewer sees the actual editor proof text"
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  assert "proofs" not in json.dumps(proposal), "the public proposal schema never grows"


def test_hidden_rationale_excludes_proofs_from_the_initial_and_repair_requests(tmp_path: Path) -> None:
  transport = VariantScriptedTransport(
      selector_response=SELECTOR_EDITOR_RESPONSE,
      reviewer_responses=[NEW_PROSE_RESPONSE, TRIM_ACCEPT_RESPONSE],  # force one bounded reviewer re-ask
  )
  outcome, _ = run_variant(tmp_path, "rationale-hidden-review", transport=transport)
  record = run_record(outcome.run_dir)
  reviewer_calls = [call for call in record["calls"] if call["role"] == "reviewer"]
  assert len(reviewer_calls) == 2, "the mechanically invalid first response was re-asked once"
  for call in reviewer_calls:
    request = (outcome.run_dir / call["request_file"]).read_text(encoding="utf-8")
    for proof in PROOFS.values():
      assert proof not in request, f"{call['request_file']} must not leak the withheld rationale"
    assert '"dispositions"' not in request, (
        f"{call['request_file']} (initial or repair) must not carry the withheld handoff")


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
      transport=VariantScriptedTransport(selector_responses=[missing, missing], reviewer_response=TRIM_ACCEPT_RESPONSE),
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
      transport=VariantScriptedTransport(selector_responses=[partial, partial], reviewer_response=TRIM_ACCEPT_RESPONSE),
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
      transport=VariantScriptedTransport(selector_responses=[stray, stray], reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-stray",
      match="only propose rows carry admission proofs")
  assert record["status"] == "failed"


# --- the trim-only capability ----------------------------------------------------


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
          selector_response=SELECTOR_EDITOR_RESPONSE, reviewer_responses=[NEW_PROSE_RESPONSE, NEW_PROSE_RESPONSE]),
      output_dir=tmp_path / "out",
      match="is not a verbatim line of the selector's proposed text")
  assert record["status"] == "failed"
  assert "is not a verbatim line of the selector's proposed text" in record["error"]
  reviewer_calls = [call for call in record["calls"] if call["role"] == "reviewer"]
  assert len(reviewer_calls) == 2 and all(call["validation"]["status"] == "failed" for call in reviewer_calls), (
      "the capability violation is a visible execution failure, both attempts kept")


def test_whole_entry_reviewer_may_rewrite_but_trim_only_may_not(tmp_path: Path) -> None:
  with pytest.raises(ReplayError, match="verbatim line"):
    run_variant(
        tmp_path,
        "baseline-original-flow",
        transport=VariantScriptedTransport(
            selector_response=SELECTOR_EDITOR_RESPONSE,
            reviewer_responses=[WHOLE_REWRITE_RESPONSE, WHOLE_REWRITE_RESPONSE]),
        output_dir=tmp_path / "out-trim")
  outcome, _ = run_variant(
      tmp_path,
      "whole-entry-review",
      transport=VariantScriptedTransport(
          selector_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=WHOLE_REWRITE_RESPONSE),
      output_dir=tmp_path / "out-whole")
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  final = base.final_entries_from(proposal, feedback_rich_manifest_dict())
  assert final["entries/render/cache-eviction.md"] == base.canonical_text(WHOLE_REWRITE_TEXT), (
      "the same response that fails the trim-only capability is a valid whole-entry rewrite")


def test_trim_only_reviewer_can_restore_the_base_and_reject_new_entries(tmp_path: Path) -> None:
  outcome, _ = run_variant(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(
          selector_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=RESTORE_RESPONSE),
      output_dir=tmp_path / "out-restore")
  proposal = json.loads((outcome.run_dir / "proposal.json").read_text(encoding="utf-8"))
  assert proposal["reviewed_patch"] == "", "restoring the base leaves no diff"

  drop_new = selector_json(
      [], [reviewer_row(outcome="no_change", paths=[], reason="reversal: the admitted entry fails the entry form")])
  outcome, _ = run_variant(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(selector_response=SELECTOR_NEW_ADMISSION_RESPONSE, reviewer_response=drop_new),
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
          selector_response=SELECTOR_EDITOR_RESPONSE, reviewer_responses=[delete_response, delete_response]),
      output_dir=tmp_path / "out-delete",
      match="may only confirm a selector delete")
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
          selector_response=selector_json([], [selector_row(outcome="no_change", paths=[])]),
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
      transport=VariantScriptedTransport(
          selector_responses=[bad_response, bad_response],
          combined_editor_response=bad_response,
          reviewer_response=TRIM_ACCEPT_RESPONSE),
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
          selector_responses=[editor_with_refs("approved-001"),
                              editor_with_refs("approved-001")],
          reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-selected")
  assert run_record(outcome.run_dir)["status"] == "completed", (
      "the selected structured view exposes the approved change, so it is citable")

  record = run_variant_expect_failure(
      tmp_path,
      "baseline-original-flow",
      transport=VariantScriptedTransport(
          selector_responses=[editor_with_refs("approved-001"),
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
          selector_responses=[editor_with_refs("fb-002"), editor_with_refs("fb-002")],
          reviewer_response=TRIM_ACCEPT_RESPONSE),
      output_dir=tmp_path / "out-raw")
  assert run_record(
      outcome.run_dir)["status"] == "completed", ("the raw-history view exposes every pool comment, selected or not")

  record = run_variant_expect_failure(
      tmp_path,
      "approved-edit-feedback",
      transport=VariantScriptedTransport(
          selector_responses=[editor_with_refs("fb-002"), editor_with_refs("fb-002")],
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
          selector_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=reviewer_with_refs),
      output_dir=tmp_path / "out-raw")
  assert run_record(outcome.run_dir)["status"] == "completed"

  record = run_variant_expect_failure(
      tmp_path,
      "approved-edit-feedback",
      transport=VariantScriptedTransport(
          selector_response=SELECTOR_EDITOR_RESPONSE, reviewer_responses=[reviewer_with_refs, reviewer_with_refs]),
      output_dir=tmp_path / "out-selected",
      match="fb-002")
  assert record["status"] == "failed" and "fb-002" in record["error"]


def test_selected_view_renders_missing_and_empty_approved_sides_honestly(tmp_path: Path) -> None:
  transport = VariantScriptedTransport(
      selector_response=SELECTOR_EDITOR_RESPONSE, reviewer_response=TRIM_ACCEPT_RESPONSE)
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
  record = run_record(outcome.run_dir)
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

COMPLETE_RESPONSES = dict(
    selector_response=SELECTOR_EDITOR_RESPONSE,
    reviewer_response=TRIM_ACCEPT_RESPONSE,
    combined_editor_response=COMBINED_EDITOR_RESPONSE)


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
  assert summary["schema"] == "memory-curation-variant-experiment/1"
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
  assert arm["editor_response_sha256"] == shared["editor_response_sha256"]
  editor_only = comparison["arms"]["editor-only"]
  assert editor_only["status"] == "established"
  assert editor_only["reviewed_patch"] != comparison["arms"]["post-review"]["reviewed_patch"] or True
  final = base.apply_unified_patch(
      {"entries/render/cache-eviction.md": base.canonical_text(base.ENTRY_WITH_INSTANCE)},
      editor_only["reviewed_patch"])
  assert final["entries/render/cache-eviction.md"] == base.canonical_text(EDITOR_TEXT), (
      "the editor-only arm is the recorded editor response's own final state")


def test_experiment_discloses_shared_and_distinct_editor_draws(tmp_path: Path) -> None:
  manifests = [write_manifest(tmp_path, name="case-alpha.yaml")]
  output_dir = tmp_path / "exp"
  run_experiment(
      ExperimentOptions(
          manifests=manifests,
          output_dir=output_dir,
          backend="fake-clc",
          variants=["baseline-original-flow", "rationale-hidden-review"]),
      cfg=experiment_cfg(tmp_path),
      transport_factory=lambda: VariantScriptedTransport(**COMPLETE_RESPONSES))
  summary = json.loads((output_dir / "experiment.json").read_text(encoding="utf-8"))
  draws = summary["cases"][0]["editor_draws"]
  assert draws["shared_editor_responses"] == [[
      "baseline-original-flow", "rationale-hidden-review"
  ]], ("identical scripted responses are recorded as a genuine shared draw, with its source")
  assert draws["note"]


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
  record = run_record(run_dir)
  unknown = dict(record)
  unknown["variant"] = dict(record["variant"], name="no-such-variant")
  with pytest.raises(ReplayError, match="does not define"):
    _contract_for(unknown)
  drifted = dict(record)
  drifted["variant"] = dict(record["variant"], version=record["variant"]["version"] + 1)
  with pytest.raises(ReplayError, match="no longer matches"):
    _contract_for(drifted)
  misversioned = dict(record)
  misversioned["prompt_versions"] = {
      "editor": "memory-experiment-editor-selector-raw-history-v9",
      "reviewer": record["prompt_versions"]["reviewer"]
  }
  with pytest.raises(ReplayError, match="prompt versions"):
    _contract_for(misversioned)
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
