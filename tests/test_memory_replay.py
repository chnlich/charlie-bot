"""Tests for the offline memory-curation replay (src/core/memory_replay/).

Every fixture is synthetic and portable: a fictional "render" project with a
cache-eviction threshold entry, one staged capture, an owning runbook, and one
prior user comment with its approved before/after texts. The fake transport
answers each stage call from a script and records every request, so the tests
assert on the observable boundary — what was sent, what was accepted, and what
the bundle contains.
"""

import hashlib
import json
import sys
from pathlib import Path

import pytest
import yaml
from conftest import backend_option

from src.core.config import CharlieBotConfig
from src.core.memory_replay import ReplayError, ReplayOptions, run_replay
from src.core.memory_replay.errors import (
    ReplayBackendError,
    ReplayIsolationError,
    ReplayManifestError,
    ReplayModelOutputError,
    ReplayValidationError,
)
from src.core.memory_replay.exchange import parse_model_output
from src.core.memory_replay.identity import approval_digest
from src.core.memory_replay.manifest import load_manifest
from src.core.memory_replay.transport import TransportResult, request_model_for
from src.core.memory_replay.validate import apply_unified_patch, build_patch, canonical_text

# --- synthetic frozen corpus ---------------------------------------------------

GUIDELINE_TEXT = (
    "Admission bar (synthetic stand-in): an entry must name a recurring action it changes and be the "
    "shortest honest statement of it; details the owning document already carries stay there.")

MECHANISM_LINE = (
    "- `render.cache_eviction.evict_below` is the single threshold behind both the eviction order and the "
    "alert; tune it on full warm-up replay, not on alert moments.")
INSTANCE_LINE = "- Warm-up instance chart-2077 idled at 12% for three hours before serving."

ENTRY_TITLE = "cache-eviction threshold: one knob gates eviction and alerting"


def entry_text(body_lines: list[str]) -> str:
  header = ["---", "scope: user", "topic: render", "audience: master, worker", f"title: {ENTRY_TITLE}", "---"]
  return "\n".join(header + body_lines) + "\n"


ENTRY_WITH_INSTANCE = entry_text([MECHANISM_LINE, INSTANCE_LINE])
ENTRY_WITHOUT_INSTANCE = entry_text([MECHANISM_LINE])

CAPTURE_TEXT = (
    "# render cache eviction (capture)\n\nThe eviction and alert thresholds are one knob "
    "(`render.cache_eviction.evict_below`); warm-up instance chart-2077 idled at 12% for hours. Record the "
    "mechanism so tuning keeps it in view.\n")

RUNBOOK_TEXT = (
    "# render runbook (synthetic)\n\nCache tuning is owned by the runbook's Tuning chapter; this document "
    "carries the current values and the tuning procedure.\n")


def base_manifest_dict() -> dict:
  return {
      "version": 1,
      "base_commit": "mem-base-0001",
      "topics": ["render"],
      "sources":
          [
              {
                  "ref": "guideline",
                  "kind": "guideline",
                  "text": GUIDELINE_TEXT
              },
              {
                  "ref": "entry-cache-eviction",
                  "kind": "entry",
                  "path": "entries/render/cache-eviction.md",
                  "text": ENTRY_WITH_INSTANCE,
              },
              {
                  "ref": "capture-eviction",
                  "kind": "candidate",
                  "text": CAPTURE_TEXT
              },
              {
                  "ref": "doc-render-runbook",
                  "kind": "document",
                  "text": RUNBOOK_TEXT
              },
          ],
      "feedback_examples":
          [
              {
                  "comment_event": "fb-001",
                  "comment_text":
                      "Drop the per-run warm-up instance names; keep the threshold mechanism and its tuning "
                      "range. fb-comment-marker",
                  "tags": ["instance-names-out", "mechanism-in"],
                  "approved_change":
                      {
                          "approved_change_ref": "approved-001",
                          "before": ENTRY_WITH_INSTANCE,
                          "after": ENTRY_WITHOUT_INSTANCE,
                      },
              }
          ],
      "themes":
          {
              "eviction":
                  {
                      "principles": ["instance-names-out"],
                      "candidate_refs": ["capture-eviction"],
                      "entry_refs": ["entry-cache-eviction"],
                      "document_refs": ["doc-render-runbook"],
                  }
          },
  }


def write_manifest(tmp_path: Path, data: dict | None = None, name: str = "manifest.yaml") -> Path:
  p = tmp_path / name
  p.write_text(yaml.safe_dump(data if data is not None else base_manifest_dict(), sort_keys=False), encoding="utf-8")
  return p


class FakeTransport:

  def __init__(self, responses: list[str]):
    self.responses = list(responses)
    self.calls: list[dict] = []

  def complete(self, *, system: str, user: str) -> TransportResult:
    self.calls.append({"system": system, "user": user})
    if not self.responses:
      raise AssertionError("fake transport ran out of scripted responses")
    text = self.responses.pop(0)
    return TransportResult(text=text, model="fake-model", prompt_tokens=100, output_tokens=42, latency_ms=5)


def replay_cfg(tmp_path: Path) -> CharlieBotConfig:
  return CharlieBotConfig(
      charliebot_home=tmp_path / "home",
      backends={
          "options":
              [
                  backend_option(
                      id="fake-clc",
                      label="Fake CLC",
                      type="charlie-code",
                      model="openai/fake-model",
                      api_base="https://replay.invalid/v1"),
                  backend_option(
                      id="fake-compat",
                      label="Fake compat",
                      type="cc-openai-compatible",
                      model="fake-model",
                      api_base="https://replay.invalid/v1"),
                  backend_option(id="fake-codex", label="Fake codex", type="codex", model="fake-model"),
              ]
      })


def run_replay_with(
    tmp_path: Path,
    responses: list[str],
    *,
    mode: str = "editor-review",
    backend: str = "fake-clc",
    manifest_path: Path | None = None,
    cfg: CharlieBotConfig | None = None,
    output_dir: Path | None = None,
):
  cfg = cfg or replay_cfg(tmp_path)
  transport = FakeTransport(responses)
  outcome = run_replay(
      ReplayOptions(
          manifest=manifest_path or write_manifest(tmp_path),
          output_dir=output_dir or tmp_path / "out",
          backend=backend,
          mode=mode),
      cfg=cfg,
      transport_factory=lambda: transport)
  return outcome, transport


def editor_json(entries: list[dict], candidates: list[dict]) -> str:
  return json.dumps({"entries": entries, "candidates": candidates})


def rewrite_op(text: str, refs: list[str] | None = None, path: str = "entries/render/cache-eviction.md") -> dict:
  return {
      "action": "rewrite",
      "path": path,
      "text": text,
      "source_refs": refs if refs is not None else ["capture-eviction", "entry-cache-eviction"],
      "reason": "merged the capture and dropped the instance detail",
  }


def keep_op(path: str = "entries/render/cache-eviction.md") -> dict:
  return {"action": "keep", "path": path, "reason": "already at the bar"}


def row(ref: str, outcome: str, paths: list[str], reason: str = "one-line reason") -> dict:
  return {"source_ref": ref, "outcome": outcome, "paths": paths, "reason": reason}


def merge_response(entry_path: str, text: str) -> str:
  return editor_json([rewrite_op(text, path=entry_path)], [row("capture-eviction", "propose", [entry_path])])


MERGE_RESPONSE = merge_response("entries/render/cache-eviction.md", ENTRY_WITHOUT_INSTANCE)


def read_proposal(run_dir: Path) -> dict:
  return json.loads((run_dir / "proposal.json").read_text(encoding="utf-8"))


def run_record(run_dir: Path) -> dict:
  return json.loads((run_dir / "run.json").read_text(encoding="utf-8"))


def final_entries_from(proposal: dict, manifest_dict: dict) -> dict:
  base = {s["path"]: canonical_text(s["text"]) for s in manifest_dict["sources"] if s["kind"] == "entry"}
  return apply_unified_patch(base, proposal["reviewed_patch"])


# --- end-to-end behavior -------------------------------------------------------


def test_editor_merge_drops_instance_details_and_keeps_mechanism(tmp_path: Path) -> None:
  outcome, _ = run_replay_with(tmp_path, [MERGE_RESPONSE, MERGE_RESPONSE])
  assert outcome.propose == 1 and outcome.changed_paths == ["entries/render/cache-eviction.md"]
  proposal = read_proposal(outcome.run_dir)
  final = final_entries_from(proposal, base_manifest_dict())
  body = final["entries/render/cache-eviction.md"]
  assert "evict_below" in body, "the mechanism text a future action needs must survive the merge"
  assert "chart-2077" not in body, "the irrelevant instance detail must leave the entry"
  assert proposal["candidate_results"][0]["outcome"] == "propose"
  assert proposal["feedback_refs"] == [{"comment_event": "fb-001", "approved_change_ref": "approved-001"}]
  assert proposal["approval_digest"] == approval_digest(proposal["base_commit"], proposal["reviewed_patch"])


def test_reviewer_reversal_updates_disposition_and_patch_together(tmp_path: Path) -> None:
  editor = editor_json(
      [rewrite_op(ENTRY_WITHOUT_INSTANCE)],
      [row("capture-eviction", "propose", ["entries/render/cache-eviction.md"], "editor-merged-the-capture")])
  reviewer = editor_json(
      [
          {
              "action": "delete",
              "path": "entries/render/cache-eviction.md",
              "reason": "reversal: the whole entry fails the synthetic bar",
          }
      ], [
          row("capture-eviction", "no_change", [], "the capture adds nothing once the entry is deleted"),
          row(
              "entries/render/cache-eviction.md", "propose", ["entries/render/cache-eviction.md"],
              "reversal: the whole entry fails the synthetic bar"),
      ])
  outcome, _ = run_replay_with(tmp_path, [editor, reviewer])
  proposal = read_proposal(outcome.run_dir)
  final = final_entries_from(proposal, base_manifest_dict())
  assert final["entries/render/cache-eviction.md"] is None, "the reversal must delete the entry"
  results = {r["source_ref"]: r for r in proposal["candidate_results"]}
  assert results["entries/render/cache-eviction.md"]["reason"].startswith("reversal:")
  assert results["capture-eviction"]["outcome"] == "no_change"
  assert outcome.propose == 1
  audit = run_record(outcome.run_dir)["editor_dispositions"]
  assert any(a["kind"] == "candidate" and "editor-merged-the-capture" in a["detail"] for a in audit), (
      "the editor's own dispositions stay in the audit record even when reversed")


def test_reviewer_request_excludes_editor_rationale_and_scoring_answers(tmp_path: Path) -> None:
  marker_reason = "EDITOR-RATIONALE-MARKER merged the capture"
  editor = editor_json(
      [rewrite_op(ENTRY_WITHOUT_INSTANCE)],
      [row("capture-eviction", "propose", ["entries/render/cache-eviction.md"], marker_reason)])
  reviewer = editor_json([keep_op()], [row("capture-eviction", "no_change", [], "kept as proposed")])
  (tmp_path / "eval-answers.json").write_text('{"score": "SCORING-ANSWER-MARKER"}', encoding="utf-8")
  outcome, transport = run_replay_with(tmp_path, [editor, reviewer])
  assert len(transport.calls) == 2
  editor_request, reviewer_request = transport.calls[0]["user"], transport.calls[1]["user"]
  assert "EDITOR-RATIONALE-MARKER" not in reviewer_request
  assert "SCORING-ANSWER-MARKER" not in reviewer_request and "SCORING-ANSWER-MARKER" not in editor_request
  assert "fb-comment-marker" in editor_request and "fb-comment-marker" in reviewer_request, (
      "the selected user comment must reach both stages")
  assert "chart-2077" in reviewer_request, "the reviewer still sees the original evidence"


def test_editor_only_shares_editor_input_and_skips_the_reviewer(tmp_path: Path) -> None:
  only_outcome, only_transport = run_replay_with(
      tmp_path, [MERGE_RESPONSE], mode="editor-only", output_dir=tmp_path / "out-only")
  review_outcome, review_transport = run_replay_with(
      tmp_path, [MERGE_RESPONSE, MERGE_RESPONSE], mode="editor-review", output_dir=tmp_path / "out-review")
  assert len(only_transport.calls) == 1 and len(review_transport.calls) == 2
  only_request = (only_outcome.run_dir / "raw" / "editor-eviction.request.txt").read_text(encoding="utf-8")
  review_request = (review_outcome.run_dir / "raw" / "editor-eviction.request.txt").read_text(encoding="utf-8")
  assert only_request == review_request, "the editor-only control must share the editor-review editor input"


def test_selected_feedback_present_in_both_editor_modes(tmp_path: Path) -> None:
  only_outcome, only_transport = run_replay_with(
      tmp_path, [MERGE_RESPONSE], mode="editor-only", output_dir=tmp_path / "out-only")
  review_outcome, review_transport = run_replay_with(
      tmp_path, [MERGE_RESPONSE, MERGE_RESPONSE], mode="editor-review", output_dir=tmp_path / "out-review")
  assert "fb-comment-marker" in only_transport.calls[0]["user"]
  assert "fb-comment-marker" in review_transport.calls[0]["user"]
  for outcome in (only_outcome, review_outcome):
    assert run_record(outcome.run_dir)["themes"][0]["selected_feedback"][0]["comment_event"] == "fb-001"


# --- retrieval and identity ----------------------------------------------------


def renamed_manifest() -> dict:
  renamed = base_manifest_dict()
  renamed["topics"] = ["plotting"]
  for source in renamed["sources"]:
    if source["kind"] == "entry":
      source["path"] = "entries/plotting/cache-eviction.md"
    source["text"] = source["text"].replace("render", "plotting")
  return renamed


def test_retrieval_selects_across_topic_names(tmp_path: Path) -> None:
  renamed_entry = ENTRY_WITHOUT_INSTANCE.replace("render", "plotting").replace("topic: render", "topic: plotting")
  renamed_response = merge_response("entries/plotting/cache-eviction.md", renamed_entry)
  first, _ = run_replay_with(
      tmp_path, [MERGE_RESPONSE, MERGE_RESPONSE], manifest_path=write_manifest(tmp_path), output_dir=tmp_path / "out-a")
  second, _ = run_replay_with(
      tmp_path, [renamed_response, renamed_response],
      manifest_path=write_manifest(tmp_path, renamed_manifest(), name="renamed.yaml"),
      output_dir=tmp_path / "out-b")
  for outcome in (first, second):
    record = run_record(outcome.run_dir)
    assert record["themes"][0]["selected_feedback"], "the tagged principle must retrieve across project names"
    assert record["themes"][0]["selected_feedback"][0]["comment_event"] == "fb-001"


def test_unrelated_feedback_is_not_selected(tmp_path: Path) -> None:
  manifest = base_manifest_dict()
  manifest["feedback_examples"][0]["tags"] = []
  manifest["feedback_examples"][0]["comment_text"] = "Unrelated: please prefer the darker plot theme."
  manifest["feedback_examples"][0]["approved_change"] = None
  manifest["themes"]["eviction"]["principles"] = []
  manifest["sources"] = [s for s in manifest["sources"] if s["kind"] == "guideline" or "text" in s]
  outcome, _ = run_replay_with(
      tmp_path, [editor_json([keep_op()], [row("capture-eviction", "no_change", [], "nothing to do")])],
      manifest_path=write_manifest(tmp_path, manifest),
      mode="editor-only")
  assert read_proposal(outcome.run_dir)["feedback_refs"] == []
  assert run_record(outcome.run_dir)["themes"][0]["selected_feedback"] == []


def test_replacing_feedback_changes_the_input_identity(tmp_path: Path) -> None:
  from src.core.memory_replay.identity import input_identity

  def identity_for(manifest_dict: dict, name: str) -> str:
    manifest = load_manifest(write_manifest(tmp_path, manifest_dict, name=name))
    return input_identity(
        manifest=manifest,
        mode="editor-review",
        model_identity={
            "backend": "b",
            "backend_type": "t",
            "model": "m"
        },
        editor_prompt_version="e",
        reviewer_prompt_version="r")

  first = identity_for(base_manifest_dict(), "m1.yaml")
  changed = base_manifest_dict()
  changed["feedback_examples"][0]["comment_text"] = (
      "Drop the per-run warm-up instance names; also keep the tuning range. fb-comment-marker")
  assert first != identity_for(changed, "m2.yaml"), "new relevant feedback must invalidate reuse"
  changed_too = base_manifest_dict()
  changed_too["feedback_examples"][0]["tags"] = ["mechanism-in"]
  assert first != identity_for(changed_too, "m3.yaml"), "a changed retrieval tag changes what the models see"


# --- reuse ---------------------------------------------------------------------


def test_identical_rerun_reuses_completed_outputs_without_model_calls(tmp_path: Path) -> None:
  first, first_transport = run_replay_with(tmp_path, [MERGE_RESPONSE, MERGE_RESPONSE])
  assert len(first_transport.calls) == 2
  before = (first.run_dir / "proposal.json").read_bytes()
  second, second_transport = run_replay_with(tmp_path, [], output_dir=tmp_path / "out")
  assert second.reused is True
  assert second_transport.calls == [], "reuse must not call the model"
  assert second.run_dir == first.run_dir
  assert (first.run_dir / "proposal.json").read_bytes() == before


def test_different_mode_or_model_gets_a_fresh_run(tmp_path: Path) -> None:
  run_replay_with(tmp_path, [MERGE_RESPONSE], mode="editor-only", output_dir=tmp_path / "out")
  outcome_only, transport_only = run_replay_with(
      tmp_path, [MERGE_RESPONSE], mode="editor-only", backend="fake-compat", output_dir=tmp_path / "out")
  assert outcome_only.reused is False
  assert len(transport_only.calls) == 1, "a different model identity invalidates reuse"


# --- valid edge outputs --------------------------------------------------------


def test_no_change_output_is_valid(tmp_path: Path) -> None:
  response = editor_json([keep_op()], [row("capture-eviction", "no_change", [], "entry already at the bar")])
  outcome, _ = run_replay_with(tmp_path, [response], mode="editor-only")
  proposal = read_proposal(outcome.run_dir)
  assert proposal["reviewed_patch"] == ""
  assert proposal["candidate_results"][0]["outcome"] == "no_change"
  assert proposal["approval_digest"] == approval_digest(proposal["base_commit"], "")
  assert outcome.changed_paths == []
  assert "(no changes)" in (outcome.run_dir / "report.html").read_text(encoding="utf-8")


def test_needs_decision_exits_success_and_shows_in_report(tmp_path: Path) -> None:
  response = editor_json([], [row("capture-eviction", "needs_decision", [], "evidence conflicts on the band")])
  outcome, _ = run_replay_with(tmp_path, [response], mode="editor-only")
  assert outcome.needs_decision == 1 and outcome.propose == 0
  report = (outcome.run_dir / "report.html").read_text(encoding="utf-8")
  assert "Needs decision" in report and "evidence conflicts on the band" in report


def test_remember_request_keeps_a_visible_disposition(tmp_path: Path) -> None:
  manifest = base_manifest_dict()
  manifest["sources"][2]["remember_request"] = True
  response = editor_json(
      [keep_op()], [
          row(
              "capture-eviction", "no_change", [], "explicit remember request stays visible; the "
              "runbook owns the detail")
      ])
  outcome, _ = run_replay_with(
      tmp_path, [response], manifest_path=write_manifest(tmp_path, manifest), mode="editor-only")
  report = (outcome.run_dir / "report.html").read_text(encoding="utf-8")
  assert "remember request" in report
  bad = editor_json([keep_op()], [row("capture-eviction", "no_change", [], "not stored")])
  with pytest.raises(ReplayValidationError, match="remember request"):
    run_replay_with(
        tmp_path, [bad],
        manifest_path=write_manifest(tmp_path, manifest, name="m2.yaml"),
        mode="editor-only",
        output_dir=tmp_path / "out-bad")


# --- mechanical validation failures --------------------------------------------


def test_unparseable_model_output_fails_visibly_with_no_proposal(tmp_path: Path) -> None:
  cfg = replay_cfg(tmp_path)
  _write_store(cfg.charliebot_home)
  snapshot = _hash_tree(cfg.charliebot_home / "memory")
  with pytest.raises(ReplayModelOutputError, match="no JSON object"):
    run_replay_with(tmp_path, ["I would suggest reviewing the entries first."], cfg=cfg)
  run_dir = next((tmp_path / "out" / "runs").iterdir())
  assert not (run_dir / "proposal.json").exists()
  assert not (run_dir / "report.html").exists()
  record = run_record(run_dir)
  assert record["status"] == "failed" and "no JSON object" in record["error"]
  assert _hash_tree(cfg.charliebot_home / "memory") == snapshot, "the live store stays byte-identical"


def test_wrong_json_shape_fails_visibly(tmp_path: Path) -> None:
  with pytest.raises(ReplayModelOutputError, match="required JSON shape"):
    run_replay_with(tmp_path, [json.dumps({"entries": "all good"})])


def test_unknown_source_ref_fails(tmp_path: Path) -> None:
  bad = editor_json(
      [rewrite_op(ENTRY_WITHOUT_INSTANCE, refs=["ghost-ref"])],
      [row("capture-eviction", "propose", ["entries/render/cache-eviction.md"])])
  with pytest.raises(ReplayValidationError, match="unknown source ref 'ghost-ref'"):
    run_replay_with(tmp_path, [bad])


def test_feedback_ids_are_citable_evidence(tmp_path: Path) -> None:
  response = editor_json(
      [rewrite_op(ENTRY_WITHOUT_INSTANCE, refs=["capture-eviction", "fb-001", "approved-001"])],
      [row("capture-eviction", "propose", ["entries/render/cache-eviction.md"])])
  outcome, _ = run_replay_with(tmp_path, [response, response])
  proposal = read_proposal(outcome.run_dir)
  assert {r["source_ref"] for r in proposal["candidate_results"]
         } == {"capture-eviction"}, ("proposal candidate_results keep the schema's source_ref space")
  assert proposal["feedback_refs"] == [{"comment_event": "fb-001", "approved_change_ref": "approved-001"}]


def test_path_traversal_is_rejected(tmp_path: Path) -> None:
  bad = editor_json(
      [rewrite_op(ENTRY_WITHOUT_INSTANCE, path="entries/render/../evil.md")],
      [row("capture-eviction", "propose", ["entries/render/../evil.md"])])
  with pytest.raises(ReplayValidationError, match="not entries/<topic>/<slug>.md"):
    run_replay_with(tmp_path, [bad])


def test_new_entry_with_undeclared_topic_fails(tmp_path: Path) -> None:
  manifest = base_manifest_dict()
  manifest["topics"] = ["render"]
  new_entry = entry_text([MECHANISM_LINE]).replace("topic: render", "topic: ghost")
  bad = editor_json(
      [
          rewrite_op(ENTRY_WITHOUT_INSTANCE), {
              "action": "new",
              "path": "entries/ghost/fresh.md",
              "text": new_entry,
              "source_refs": ["capture-eviction"],
              "reason": "new topic entry",
          }
      ], [row("capture-eviction", "propose", ["entries/ghost/fresh.md", "entries/render/cache-eviction.md"])])
  with pytest.raises(ReplayValidationError, match="violates the entry format"):
    run_replay_with(tmp_path, [bad], manifest_path=write_manifest(tmp_path, manifest))


def test_changed_path_without_evidence_mapping_fails(tmp_path: Path) -> None:
  bad = editor_json([rewrite_op(ENTRY_WITHOUT_INSTANCE)], [row("capture-eviction", "no_change", [], "kept")])
  with pytest.raises(ReplayValidationError, match="no propose disposition"):
    run_replay_with(tmp_path, [bad], mode="editor-only")


def test_propose_path_that_does_not_change_fails(tmp_path: Path) -> None:
  bad = editor_json([keep_op()], [row("capture-eviction", "propose", ["entries/render/cache-eviction.md"])])
  with pytest.raises(ReplayValidationError, match="the final diff does not change"):
    run_replay_with(tmp_path, [bad], mode="editor-only")


def test_missing_candidate_disposition_fails(tmp_path: Path) -> None:
  bad = editor_json([keep_op()], [])
  with pytest.raises(ReplayValidationError, match="no disposition row"):
    run_replay_with(tmp_path, [bad])


def test_unappliable_patch_fails_visibly(tmp_path: Path) -> None:
  base = {"entries/t/a.md": "different\n"}
  with pytest.raises(ReplayValidationError, match="does not match the base"):
    apply_unified_patch(
        base,
        build_patch({"entries/t/a.md": "line one\nline two\n"}, {"entries/t/a.md": "line one\nline two changed\n"}))


def test_generated_patch_round_trips_every_shape(tmp_path: Path) -> None:
  base = {
      "entries/t/mod.md": "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
      "entries/t/del.md": "x\ny\n",
  }
  final = {
      "entries/t/mod.md": "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\n",
      "entries/t/new.md": "a\nb\n",
  }
  patch = build_patch(base, final)
  applied = apply_unified_patch(base, patch)
  assert applied["entries/t/mod.md"] == final["entries/t/mod.md"]
  assert applied["entries/t/new.md"] == final["entries/t/new.md"]
  assert applied["entries/t/del.md"] is None


def test_approval_digest_binds_base_and_patch(tmp_path: Path) -> None:
  assert approval_digest("b", "p") != approval_digest("b", "p2")
  assert approval_digest("b", "p") != approval_digest("b2", "p")
  outcome, _ = run_replay_with(tmp_path, [MERGE_RESPONSE, MERGE_RESPONSE])
  proposal = read_proposal(outcome.run_dir)
  assert proposal["approval_digest"] == approval_digest(proposal["base_commit"], proposal["reviewed_patch"])


# --- report --------------------------------------------------------------------


def test_report_renders_the_actual_patch_and_escapes_markup(tmp_path: Path) -> None:
  hostile = entry_text([MECHANISM_LINE, "- Warm-up <script>alert('x')</script> idled at 12%."])
  manifest = base_manifest_dict()
  for source in manifest["sources"]:
    if source.get("path") == "entries/render/cache-eviction.md":
      source["text"] = hostile
  proposed = entry_text([MECHANISM_LINE, "- Warm-up idled at 12%."])
  response = editor_json(
      [rewrite_op(proposed)], [row("capture-eviction", "propose", ["entries/render/cache-eviction.md"])])
  outcome, _ = run_replay_with(tmp_path, [response, response], manifest_path=write_manifest(tmp_path, manifest))
  report = (outcome.run_dir / "report.html").read_text(encoding="utf-8")
  assert "&lt;script&gt;alert(&#x27;x&#x27;)&lt;/script&gt;" in report
  assert "<script>alert" not in report
  assert "-Warm-up" in report or "- Warm-up" in report, "the report must render the actual final patch"
  assert "evict_below" in report


# --- isolation -----------------------------------------------------------------


def _write_store(home: Path) -> None:
  memory_dir = home / "memory"
  (memory_dir / "entries" / "render").mkdir(parents=True)
  (memory_dir / "staging").mkdir()
  (memory_dir / "topics").write_text("render\n", encoding="utf-8")
  (memory_dir / "entries" / "render" / "cache-eviction.md").write_text(ENTRY_WITH_INSTANCE, encoding="utf-8")
  (memory_dir / "staging" / "capture.md").write_text(CAPTURE_TEXT, encoding="utf-8")


def _hash_tree(root: Path) -> dict[str, str]:
  return {
      str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
      for p in sorted(root.rglob("*"))
      if p.is_file()
  }


def test_replay_leaves_the_live_store_byte_identical(tmp_path: Path) -> None:
  cfg = replay_cfg(tmp_path)
  _write_store(cfg.charliebot_home)
  before = _hash_tree(cfg.charliebot_home / "memory")
  outcome, _ = run_replay_with(tmp_path, [MERGE_RESPONSE, MERGE_RESPONSE], cfg=cfg)
  assert _hash_tree(cfg.charliebot_home / "memory") == before
  assert (outcome.run_dir / "proposal.json").exists()


@pytest.mark.parametrize(
    ("output_rel"),
    ["home/memory", "home/memory/proposals", "home", "out"],
)
def test_output_roots_overlapping_store_or_inputs_are_rejected(tmp_path: Path, output_rel: str) -> None:
  cfg = replay_cfg(tmp_path)
  _write_store(cfg.charliebot_home)
  manifest_path = write_manifest(tmp_path)
  output_dir = tmp_path / output_rel
  if output_rel == "out":
    # the manifest itself lives inside the requested output root
    manifest_path = output_dir / "manifest.yaml"
    output_dir.mkdir(parents=True)
    manifest_path.write_text(yaml.safe_dump(base_manifest_dict(), sort_keys=False), encoding="utf-8")
  with pytest.raises(ReplayIsolationError, match="overlaps"):
    run_replay(
        ReplayOptions(manifest=manifest_path, output_dir=output_dir, backend="fake-clc", mode="editor-only"),
        cfg=cfg,
        transport_factory=lambda: FakeTransport([MERGE_RESPONSE]))
  assert not (tmp_path / "out" / "runs").exists() or not list((tmp_path / "out" / "runs").iterdir())


# --- backend selection ---------------------------------------------------------


def test_unconfigured_backend_fails_visibly(tmp_path: Path) -> None:
  with pytest.raises(ReplayBackendError, match="not in backends.options"):
    run_replay_with(tmp_path, [MERGE_RESPONSE], backend="no-such-backend")
  assert not (tmp_path / "out" / "runs").exists()


def test_unsupported_backend_type_fails_without_substitution(tmp_path: Path) -> None:
  with pytest.raises(ReplayBackendError, match="replay supports"):
    run_replay_with(tmp_path, [MERGE_RESPONSE], backend="fake-codex")
  assert not (tmp_path / "out" / "runs").exists()


def test_request_model_follows_each_backend_type_convention() -> None:
  clc = backend_option(
      id="clc", label="CLC", type="charlie-code", model="openai/served-name", api_base="https://x.invalid/v1")
  compat = backend_option(
      id="compat", label="compat", type="cc-openai-compatible", model="served-name", api_base="https://x.invalid/v1")
  assert request_model_for(clc) == "served-name"
  assert request_model_for(compat) == "served-name"
  with pytest.raises(ReplayBackendError, match="replay supports"):
    request_model_for(backend_option(id="codex", label="codex", type="codex", model="m"))


# --- manifest validation -------------------------------------------------------


def test_manifest_rejects_unknown_keys_so_eval_metadata_cannot_ride_along(tmp_path: Path) -> None:
  manifest = base_manifest_dict()
  manifest["sources"][2]["scoring_answer"] = "the entry should be deleted"
  with pytest.raises(ReplayManifestError, match="invalid"):
    load_manifest(write_manifest(tmp_path, manifest))


def test_manifest_requires_candidates_guideline_and_shape(tmp_path: Path) -> None:
  base = base_manifest_dict()
  no_candidates = {
      **base, "sources": [s for s in base["sources"] if s["kind"] != "candidate"],
      "themes": {
          "eviction": {
              **base["themes"]["eviction"], "candidate_refs": []
          }
      }
  }
  with pytest.raises(ReplayManifestError, match="no candidate sources"):
    load_manifest(write_manifest(tmp_path, no_candidates))

  no_guideline = {**base, "sources": [s for s in base["sources"] if s["kind"] != "guideline"]}
  with pytest.raises(ReplayManifestError, match="no guideline source"):
    load_manifest(write_manifest(tmp_path, no_guideline, name="m2.yaml"))

  entry_no_path = {**base, "sources": [{**s, "path": None} if s["kind"] == "entry" else s for s in base["sources"]]}
  with pytest.raises(ReplayManifestError, match="entries/<topic>/<slug>.md"):
    load_manifest(write_manifest(tmp_path, entry_no_path, name="m3.yaml"))

  duplicate_refs = {**base, "sources": base["sources"] + [base["sources"][2]]}
  with pytest.raises(ReplayManifestError, match="duplicate source ref"):
    load_manifest(write_manifest(tmp_path, duplicate_refs, name="m4.yaml"))

  unassigned = {
      **base,
      "themes":
          {
              "eviction":
                  {
                      "principles": ["instance-names-out"],
                      "candidate_refs": [],
                      "entry_refs": ["entry-cache-eviction"],
                      "document_refs": ["doc-render-runbook"]
                  }
          },
  }
  with pytest.raises(ReplayManifestError, match="assigns no candidates"):
    load_manifest(write_manifest(tmp_path, unassigned, name="m5.yaml"))


def test_manifest_external_file_source(tmp_path: Path) -> None:
  (tmp_path / "frozen").mkdir()
  (tmp_path / "frozen" / "capture.txt").write_text(CAPTURE_TEXT, encoding="utf-8")
  manifest = base_manifest_dict()
  manifest["sources"][2] = {"ref": "capture-eviction", "kind": "candidate", "file": "frozen/capture.txt"}
  loaded = load_manifest(write_manifest(tmp_path, manifest))
  source = loaded.source("capture-eviction")
  assert source.text == CAPTURE_TEXT
  assert source.file == tmp_path / "frozen" / "capture.txt"


# --- response parser unit ------------------------------------------------------


def test_parse_model_output_tolerates_a_code_fence() -> None:
  output = parse_model_output(f"```json\n{MERGE_RESPONSE}\n```", role="editor[t]")
  assert output.entries[0].action == "rewrite"
  assert output.candidates[0].source_ref == "capture-eviction"


def test_parse_model_output_rejects_garbage() -> None:
  with pytest.raises(ReplayModelOutputError):
    parse_model_output("no json here", role="editor[t]")


# --- transport -----------------------------------------------------------------


class _StubResponse:

  def __init__(self, status_code=200, payload=None, text=""):
    self.status_code = status_code
    self._payload = payload
    self.text = text

  def json(self):
    if self._payload is None:
      raise ValueError("no body")
    return self._payload


class _StubClient:

  response: _StubResponse = _StubResponse(
      payload={
          "choices": [{
              "message": {
                  "content": "ok"
              }
          }],
          "usage": {
              "prompt_tokens": 11,
              "completion_tokens": 7
          }
      })
  requests: list[dict] = []

  def __init__(self, **kwargs):
    self.kwargs = kwargs

  def __enter__(self):
    return self

  def __exit__(self, *exc):
    return False

  def post(self, url, json=None, headers=None):
    _StubClient.requests.append({"url": url, "json": json, "headers": headers})
    return type(self).response


def test_transport_sends_content_only_request_and_reads_usage(monkeypatch: pytest.MonkeyPatch) -> None:
  from src.core.memory_replay import transport as transport_module

  option = backend_option(
      id="clc", label="CLC", type="charlie-code", model="openai/served-name", api_base="https://x.invalid/v1")
  sent = {}

  class RecordingClient(_StubClient):

    def post(self, url, json=None, headers=None):
      sent.update({"url": url, "json": json, "headers": headers})
      return _StubClient.response

  monkeypatch.setattr(transport_module.httpx, "Client", RecordingClient)
  result = transport_module.OpenAICompatibleTransport.from_config(option).complete(system="s", user="u")
  assert sent["url"] == "https://x.invalid/v1/chat/completions"
  assert sent["json"]["model"] == "served-name"
  assert [m["role"] for m in sent["json"]["messages"]] == ["system", "user"]
  assert "tools" not in sent["json"], "replay transport offers no tools at all"
  assert sent["headers"] == {"Content-Type": "application/json"}
  assert result.text == "ok" and result.output_tokens == 7 and result.prompt_tokens == 11


def test_transport_authorizes_from_credentials_and_never_leaks_them(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
  from src.core import config as core_config
  from src.core.memory_replay import transport as transport_module

  secret = "super-secret-key-value"
  monkeypatch.setattr(
      transport_module, "get_credentials",
      lambda: core_config.Credentials(path=tmp_path / "credentials.yaml", sections={"k3": {
          "api_key": secret
      }}))
  option = backend_option(
      id="clc",
      label="CLC",
      type="charlie-code",
      model="openai/served-name",
      api_base="https://x.invalid/v1",
      credential="k3")
  built = transport_module.OpenAICompatibleTransport.from_config(option)
  assert built._headers.get("Authorization") == f"Bearer {secret}"

  class FailingClient(_StubClient):

    response = _StubResponse(status_code=503, payload=None, text="upstream exploded")

  monkeypatch.setattr(transport_module.httpx, "Client", FailingClient)
  with pytest.raises(ReplayError, match="HTTP 503") as exc_info:
    built.complete(system="s", user="u")
  assert secret not in str(exc_info.value), "the transport never surfaces the credential"


# --- CLI -----------------------------------------------------------------------


def _write_cli_profile_config() -> None:
  from src.core import config as core_config

  profile = core_config.charliebot_home_dir()
  profile.mkdir(parents=True, exist_ok=True)
  (profile / "config.yaml").write_text(
      "backends:\n"
      "  options:\n"
      "    - id: fake-clc\n"
      "      label: Fake CLC\n"
      "      type: charlie-code\n"
      "      model: openai/fake-model\n"
      "      api_base: https://replay.invalid/v1\n",
      encoding="utf-8")


class _StubTransportClass:

  instances: list[FakeTransport] = []

  def __init__(self, transport: FakeTransport):
    self._transport = transport

  @classmethod
  def from_config(cls, option, cfg=None):
    instance = FakeTransport([MERGE_RESPONSE])
    _StubTransportClass.instances.append(instance)
    return instance

  def complete(self, **kwargs):
    return self._transport.complete(**kwargs)


def test_cli_replay_end_to_end(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  import tests.conftest as conftest_module
  from src.cli import memory as memory_cli
  from src.core.memory_replay import runner as runner_module

  _write_cli_profile_config()
  conftest_module.reset_config_caches()
  monkeypatch.setattr(runner_module, "OpenAICompatibleTransport", _StubTransportClass)
  manifest = write_manifest(tmp_path)
  output_dir = tmp_path / "cli-out"

  monkeypatch.setattr(
      sys, "argv", [
          "charliebot memory", "replay", "--input",
          str(manifest), "--output-dir",
          str(output_dir), "--backend", "fake-clc", "--mode", "editor-only"
      ])
  memory_cli.main()

  out = capsys.readouterr().out
  assert "replay complete: editor-only" in out
  run_dir = next((output_dir / "runs").iterdir())
  assert (run_dir / "proposal.json").exists() and (run_dir / "report.html").exists()
  assert "dispositions: propose 1" in out


def test_cli_replay_rejects_missing_args_without_model_call(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  from src.cli import memory as memory_cli
  from src.core.memory_replay import runner as runner_module

  def explode(**kwargs):
    raise AssertionError("no model call may start on missing arguments")

  monkeypatch.setattr(runner_module, "OpenAICompatibleTransport", explode)
  monkeypatch.setattr(sys, "argv", ["charliebot memory", "replay", "--backend", "fake-clc", "--mode", "editor-only"])
  with pytest.raises(SystemExit) as exc_info:
    memory_cli.main()
  assert exc_info.value.code == 2
  assert "the following arguments are required" in capsys.readouterr().err


def test_cli_replay_reports_errors_without_traceback(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
  import tests.conftest as conftest_module
  from src.cli import memory as memory_cli

  _write_cli_profile_config()
  conftest_module.reset_config_caches()
  manifest = write_manifest(tmp_path)
  monkeypatch.setattr(
      sys, "argv", [
          "charliebot memory", "replay", "--input",
          str(manifest), "--output-dir",
          str(tmp_path / "out"), "--backend", "unknown-backend", "--mode", "editor-only"
      ])
  with pytest.raises(SystemExit) as exc_info:
    memory_cli.main()
  assert exc_info.value.code == 1
  err = capsys.readouterr().err
  assert err.startswith("error: ") and "not in backends.options" in err
