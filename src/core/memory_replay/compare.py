"""Paired editor/reviewer comparison over one recorded editor-review run.

``charliebot memory compare`` is the offline, deterministic second half of the
replay evaluation. It takes one recorded editor-review run and derives BOTH
arms of the comparison from the same recorded model outputs: the editor-only
alternative from the exact editor response the reviewer consumed, and the
post-review arm from the recorded reviewer responses. No model call is made
and nothing outside the requested output directory is written.

Provenance before content. The comparison first re-establishes that its inputs
are the recorded run's inputs, and refuses to proceed when they are not:

- the frozen-input bundle copy (or, for runs recorded before self-contained
  bundles existed, the manifest at the path the run record names) must
  reproduce the run's recorded input identity;
- every bundle file the run hashed at write time must still match that hash;
- the recorded attempt chain of every stage must reconstruct: each attempt's
  request must be byte-identical to the request its contract builds (the
  stage's base request for attempt 1, the bounded repair request — same
  evidence, the stage's own previous raw response, its recorded validation
  errors — for a re-ask), the chosen attempt must be the last one, and a
  response recorded as failed must still fail mechanical validation;
- each recorded editor request must be byte-identical to the request the
  recorded contract builds from the frozen inputs and the recorded feedback
  selection, and each recorded reviewer request byte-identical to the request
  built from the frozen inputs and the recorded editor response — which is
  the proof that the reviewer actually consumed that editor response, not
  merely that two JSON files exist;
- the recorded system-prompt fingerprints must match the prompts of the
  recorded contract;
- a recorded proposal must still match the finalization of the recorded
  reviewer responses (base, sources, patch, digest, final dispositions).

Version dispatch. The run's recorded prompt versions select the exchange
contract used for every check above: v3 runs are read under the v3 contract,
v2 runs under the v2 contract with its original operation and validation
meanings, and anything else fails explicitly. A v2 run's known failed arms
stay failed — v3's wider citation allowance does not reach back into old
records. Runs recorded before this provenance metadata existed are supported
when every check their record can support passes, and the comparison explicitly
declares what such a record never saved instead of silently certifying it.

Each arm is then parsed, mechanically validated, and finalized with the same
helpers the runner used. A stage whose recorded response fails to parse or
validate is reported as a failed arm — never substituted with empty or
no-change output — while the denominators (themes and input candidates) stay
fixed. Usage counts every recorded attempt, failed repairs included. A
recovered response can change an arm's judgment; that is stage
execution/recovery, not independent-review quality gain, and the report says
so. Quality is reported as unjudged: fewer lines, more deletions, or fewer
proposed paths do not establish better content. The exit status communicates
mechanical execution and format success only.
"""

import json
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

import structlog

from src.core.config import CharlieBotConfig, get_config
from src.core.memory_replay import errors, exchange_v2, variants
from src.core.memory_replay.exchange import (
    EDITOR_PROMPT_VERSION,
    EDITOR_SYSTEM,
    REVIEWER_PROMPT_VERSION,
    REVIEWER_SYSTEM,
    ThemeOutput,
    build_editor_request,
    build_repair_request,
    build_reviewer_request,
    parse_model_output,
)
from src.core.memory_replay.identity import approval_digest, canonical_bytes, input_identity, sha256_hex
from src.core.memory_replay.manifest import Manifest, load_manifest
from src.core.memory_replay.report import _e, _page, _row
from src.core.memory_replay.retrieval import FeedbackSelection
from src.core.memory_replay.runner import (
    MAX_STAGE_RESPONSES,
    PROPOSAL_SCHEMA,
    _aggregate_candidate_results,
    _require_store_disjoint_output_root,
    _timestamp,
    write_pretty_json,
)
from src.core.memory_replay.validate import (
    build_patch,
    canonical_text,
    finalize,
    validate_theme_output,
    validate_theme_output_v2,
)

log = structlog.get_logger()

COMPARISON_SCHEMA = "memory-replay-comparison/1"
COMPARISON_NOTE = (
    "Paired editor/reviewer comparison: both arms derive from the single recorded editor response "
    "of the source run. This is not a rerun of the old production selector/reviewer pipeline; that "
    "baseline and broader held-out evaluation are separate work.")
QUALITY_NOTE = (
    "No semantic quality judgment is made here: fewer lines, more deletions, or fewer proposed paths "
    "do not establish better quality. Evaluate the content against separately stored user judgments.")
_DENOMINATORS_NOTE = "fixed denominators from the frozen inputs; failed arms stay counted here"


@dataclass
class CompareOptions:
  """The CLI-facing options: which recorded run to compare, where to write the result."""

  run_dir: Path
  output_dir: Path


@dataclass(frozen=True)
class ExchangeContract:
  """The exchange contract one recorded run was made under, selected by its recorded versions.

  Everything the comparison checks against the contract — the system prompts it
  fingerprints, the request builders it reconstructs with, the validator that
  gives recorded responses their meanings — comes from here, so a v2 record is
  interpreted with v2 meanings and a v3 record with v3 meanings, and no other
  version is interpreted at all.
  """

  name: str
  editor_system: str
  reviewer_system: str
  build_editor_request: Callable[..., str]
  build_reviewer_request: Callable[..., str]
  parse_editor_output: Callable[..., ThemeOutput]
  parse_reviewer_output: Callable[..., ThemeOutput]
  validate_editor: Callable[..., None]
  validate_reviewer: Callable[..., None]
  bounded_recovery: bool


def _permissive_role_validators(validate: Callable[..., None]) -> tuple[Callable, Callable]:
  """The v2/v3 validators are role-agnostic; the reviewer's takes the consumed editor output
  (unused there) so one call shape serves every contract."""

  def editor_validate(output, *, role, manifest, theme, selections):
    del selections
    validate(output, role=role, manifest=manifest, theme=theme)

  def reviewer_validate(output, *, role, manifest, theme, selections, editor_output):
    del selections, editor_output
    validate(output, role=role, manifest=manifest, theme=theme)

  return editor_validate, reviewer_validate


def _contract_for(record: dict) -> ExchangeContract:
  if record.get("variant"):
    return _experiment_contract_view(variants.contract_for_record(record))
  versions = record.get("prompt_versions") or {}
  editor_version, reviewer_version = versions.get("editor"), versions.get("reviewer")
  if editor_version == EDITOR_PROMPT_VERSION and reviewer_version == REVIEWER_PROMPT_VERSION:
    editor_validate, reviewer_validate = _permissive_role_validators(validate_theme_output)
    return ExchangeContract(
        name="v3",
        editor_system=EDITOR_SYSTEM,
        reviewer_system=REVIEWER_SYSTEM,
        build_editor_request=build_editor_request,
        build_reviewer_request=build_reviewer_request,
        parse_editor_output=parse_model_output,
        parse_reviewer_output=parse_model_output,
        validate_editor=editor_validate,
        validate_reviewer=reviewer_validate,
        bounded_recovery=True)
  if (editor_version == exchange_v2.EDITOR_PROMPT_VERSION and reviewer_version == exchange_v2.REVIEWER_PROMPT_VERSION):
    editor_validate, reviewer_validate = _permissive_role_validators(validate_theme_output_v2)
    return ExchangeContract(
        name="v2",
        editor_system=exchange_v2.EDITOR_SYSTEM,
        reviewer_system=exchange_v2.REVIEWER_SYSTEM,
        build_editor_request=exchange_v2.build_editor_request,
        build_reviewer_request=exchange_v2.build_reviewer_request,
        parse_editor_output=parse_model_output,
        parse_reviewer_output=parse_model_output,
        validate_editor=editor_validate,
        validate_reviewer=reviewer_validate,
        bounded_recovery=False)
  raise errors.ReplayError(
      f"unsupported replay prompt version(s): editor {editor_version!r}, reviewer {reviewer_version!r}; "
      f"this comparison reads {EDITOR_PROMPT_VERSION!r} (v3), "
      f"{exchange_v2.EDITOR_PROMPT_VERSION!r} (v2), and the experimental variant contracts")


def _experiment_contract_view(contract: variants.ExperimentContract) -> ExchangeContract:
  """The comparison-side view of one experimental variant contract.

  The variant module stays the single home: this adapter only reshapes its error-listing
  validators into the raising form the comparison arm establishment uses, and threads the
  recorded editor output into the reviewer validation (the trim-only capability check needs the
  exact selector text the reviewer's responses derive from).
  """

  def editor_validate(output, *, role, manifest, theme, selections):
    found = contract.editor_errors(
        output, role=role, manifest=manifest, theme=theme, selections=selections, editor_output=None)
    if found:
      raise errors.ReplayValidationError("\n".join(found))

  def reviewer_validate(output, *, role, manifest, theme, selections, editor_output):
    found = contract.reviewer_errors(
        output, role=role, manifest=manifest, theme=theme, selections=selections, editor_output=editor_output)
    if found:
      raise errors.ReplayValidationError("\n".join(found))

  return ExchangeContract(
      name=f"experiment:{contract.name}",
      editor_system=contract.editor_system,
      reviewer_system=contract.reviewer_system,
      build_editor_request=contract.build_editor_request,
      build_reviewer_request=contract.build_reviewer_request,
      parse_editor_output=contract.parse_editor_output,
      parse_reviewer_output=contract.parse_reviewer_output,
      validate_editor=editor_validate,
      validate_reviewer=reviewer_validate,
      bounded_recovery=True)


@dataclass
class StageAttempt:
  """One recorded stage attempt: its raw request/response and the recorded validation outcome."""

  attempt: int
  request: str | None
  response: str | None
  chosen: bool
  validation: dict


@dataclass
class CompareOutcome:
  comparison_path: Path
  report_path: Path
  editor_status: str
  reviewer_status: str


def run_comparison(
    options: CompareOptions,
    *,
    cfg: CharlieBotConfig | None = None,
    now: datetime | None = None,
) -> CompareOutcome:
  """Compare one recorded editor-review run end to end; every failure is a :class:`ReplayError`."""
  try:
    outcome = _run_comparison(options, cfg=cfg or get_config(), now=now)
  except errors.ReplayError as e:
    log.error("memory_compare_failed", run_dir=str(options.run_dir), error=str(e))
    raise
  except Exception as e:
    raise errors.ReplayError(f"{type(e).__name__}: {e}") from e
  log.info(
      "memory_compare_completed",
      run_dir=str(options.run_dir),
      output_dir=str(options.output_dir),
      editor_arm=outcome.editor_status,
      reviewer_arm=outcome.reviewer_status)
  return outcome


def _run_comparison(options: CompareOptions, *, cfg: CharlieBotConfig, now: datetime | None) -> CompareOutcome:
  run_dir = options.run_dir
  record = _load_run_record(run_dir)
  if record.get("mode") != "editor-review":
    raise errors.ReplayError(
        f"comparison contrasts the two arms of an editor-review run; {run_dir} recorded mode "
        f"{record.get('mode')!r}")
  if record.get("status") not in ("completed", "failed"):
    raise errors.ReplayError(
        f"the run at {run_dir} has not settled (status {record.get('status')!r}); comparison reads a "
        "completed or failed run")

  contract = _contract_for(record)
  verification: dict = {}
  provenance = _new_provenance()

  _require_disjoint_comparison_output(options.output_dir, run_dir, cfg)
  verification["artifacts"] = _verify_recorded_artifacts(run_dir, record)
  manifest, manifest_verification = _load_verified_manifest(run_dir, record)
  verification.update(manifest_verification)
  _verify_source_snapshots(run_dir, manifest, verification)
  _verify_prompt_fingerprints(record, contract, verification)

  selections = _recorded_selections(record, manifest)
  editor_stage = _read_stage_attempts(run_dir, manifest, record, "editor", contract)
  reviewer_stage = _read_stage_attempts(run_dir, manifest, record, "reviewer", contract)

  run_error = record.get("error")
  editor_outputs, editor_parse_errors = _parse_chosen_outputs(
      manifest, editor_stage, role="editor", contract=contract, run_error=run_error)
  reviewer_outputs, reviewer_parse_errors = _parse_chosen_outputs(
      manifest, reviewer_stage, role="reviewer", contract=contract, run_error=run_error)

  _verify_stage_chain(
      contract=contract,
      manifest=manifest,
      selections=selections,
      role="editor",
      stage_attempts=editor_stage,
      chosen_outputs=editor_outputs,
      verification=verification)
  _verify_stage_chain(
      contract=contract,
      manifest=manifest,
      selections=selections,
      role="reviewer",
      stage_attempts=reviewer_stage,
      chosen_outputs=editor_outputs,
      verification=verification)

  _fill_provenance(provenance, manifest, editor_stage, reviewer_stage)

  editor_arm = _establish_arm(
      manifest,
      editor_outputs,
      editor_parse_errors,
      role="editor",
      contract=contract,
      selections=selections,
      editor_outputs=editor_outputs)
  reviewer_arm = _establish_arm(
      manifest,
      reviewer_outputs,
      reviewer_parse_errors,
      role="reviewer",
      contract=contract,
      selections=selections,
      editor_outputs=editor_outputs)
  if reviewer_arm["status"] == "failed" and record["status"] == "completed":
    # A completed run's reviewer responses validated when it ran; if they no longer do, the
    # bundle changed or the validation contract drifted, and its proposal cannot be cross-checked.
    raise errors.ReplayError(
        f"the recorded reviewer responses of the completed run at {run_dir} no longer validate: "
        f"{reviewer_arm['error']}")
  verification["proposal"] = _verify_proposal(run_dir, manifest, record, reviewer_arm, selections)
  verification["limitations"] = _verification_limitations(verification)

  comparison = {
      "schema": COMPARISON_SCHEMA,
      "kind": "paired-editor-reviewer-comparison",
      "created_at": _timestamp(now),
      "note": COMPARISON_NOTE,
      "model_calls_made": 0,
      "source_run": _source_run_section(run_dir, record, manifest, contract),
      "verification": verification,
      "provenance": provenance,
      "recovery": _recovery_section(manifest, editor_stage, reviewer_stage),
      "denominators": {
          **_denominators(manifest), "note": _DENOMINATORS_NOTE
      },
      "arms": {
          "editor-only": editor_arm,
          "post-review": reviewer_arm,
      },
      "usage": _usage(record),
      "quality": {
          "status": "unjudged",
          "editor_only": None,
          "post_review": None,
          "note": QUALITY_NOTE,
      },
  }
  options.output_dir.mkdir(parents=True, exist_ok=True)
  comparison_path = options.output_dir / "comparison.json"
  report_path = options.output_dir / "report.html"
  write_pretty_json(comparison_path, comparison)
  report_path.write_text(render_comparison_report(comparison), encoding="utf-8")
  return CompareOutcome(
      comparison_path=comparison_path,
      report_path=report_path,
      editor_status=editor_arm["status"],
      reviewer_status=reviewer_arm["status"],
  )


# --- provenance: prove the inputs are the recorded run's inputs -----------------


def _load_run_record(run_dir: Path) -> dict:
  record_path = run_dir / "run.json"
  if not record_path.is_file():
    raise errors.ReplayError(f"{run_dir} is not a replay run directory: run.json is missing")
  try:
    return json.loads(record_path.read_text(encoding="utf-8"))
  except (OSError, ValueError) as e:
    raise errors.ReplayError(f"run record {record_path} is unreadable: {e}") from e


def _load_verified_manifest(run_dir: Path, record: dict) -> tuple[Manifest, dict]:
  """Load the frozen inputs and prove they reproduce the run's recorded input identity.

  Self-contained runs carry the fully inlined manifest under ``frozen/manifest.yaml``,
  hashed into the run record at write time; the original manifest file may have moved
  or changed without invalidating the comparison, and its state is recorded. Runs
  recorded before that persistence existed have only the recorded manifest path: the
  file must still be there and unchanged, or the comparison cannot be established.
  """
  frozen = record.get("frozen_inputs")
  if frozen:
    rel = frozen.get("manifest") or "frozen/manifest.yaml"
    frozen_path = run_dir / rel
    if not frozen_path.is_file():
      raise errors.ReplayError(
          f"the run bundle at {run_dir} records frozen inputs at {rel} but the file is missing; "
          "the bundle changed after the run")
    try:
      manifest = load_manifest(frozen_path)
    except errors.ReplayManifestError as e:
      raise errors.ReplayError(f"the run's frozen manifest copy {frozen_path} is invalid: {e}") from e
    verification = {
        "manifest_inputs":
            {
                "source": "run-bundle-frozen",
                "frozen_manifest": rel,
                "external_manifest": _external_manifest_state(record, manifest),
            }
    }
  else:
    recorded = record.get("manifest")
    if not recorded:
      raise errors.ReplayError(
          f"the run record at {run_dir} names no manifest and carries no frozen inputs; the comparison "
          "cannot be established")
    manifest_path = Path(recorded)
    if not manifest_path.is_file():
      raise errors.ReplayError(
          f"the run is not self-contained and its recorded manifest {recorded} no longer exists; the "
          "comparison cannot be established (self-contained runs keep a frozen copy in the bundle)")
    try:
      manifest = load_manifest(manifest_path)
    except errors.ReplayManifestError as e:
      raise errors.ReplayError(
          f"the run's recorded manifest {recorded} changed or became invalid: {e}; the comparison cannot "
          "be established") from e
    verification = {
        "manifest_inputs":
            {
                "source": "recorded-manifest-path",
                "recorded_manifest": str(manifest_path),
                "external_manifest": {
                    "state": "is-the-verification-source",
                    "path": recorded,
                },
            }
    }
  actual = _recorded_identity(manifest, record)
  if actual != record.get("input_identity"):
    raise errors.ReplayError(
        "the frozen inputs do not reproduce the run's recorded input identity "
        f"({str(actual)[:16]} != {str(record.get('input_identity'))[:16]}); the manifest or the run record "
        "changed, so the two alternatives cannot be established")
  verification["manifest_inputs"]["identity_verified"] = True
  return manifest, verification


def _recorded_identity(manifest: Manifest, record: dict) -> str:
  """The input identity the recorded run must have computed over these inputs."""
  return input_identity(
      manifest=manifest,
      mode=record["mode"],
      model_identity=record["model"],
      editor_prompt_version=record["prompt_versions"]["editor"],
      reviewer_prompt_version=record["prompt_versions"]["reviewer"],
      variant=variants.identity_fields_from_record(record) if record.get("variant") else None)


def _external_manifest_state(record: dict, manifest: Manifest) -> dict:
  """Observational state of the originally recorded manifest file; never a comparison failure."""
  recorded = record.get("manifest")
  if not recorded:
    return {"state": "not-recorded"}
  path = Path(recorded)
  if not path.is_file():
    return {"state": "absent", "path": recorded}
  try:
    external = load_manifest(path)
  except errors.ReplayManifestError as e:
    return {"state": "unreadable", "path": recorded, "detail": str(e)}
  if _recorded_identity(external, record) == _recorded_identity(manifest, record):
    return {"state": "matches-the-recorded-identity", "path": recorded}
  return {
      "state": "differs-from-the-recorded-identity",
      "path": recorded,
      "detail": "the original manifest moved or changed; the comparison uses the run bundle's frozen copy",
  }


def _verify_recorded_artifacts(run_dir: Path, record: dict) -> dict:
  """Every bundle file the run hashed at write time must still match that hash."""
  integrity = record.get("bundle_integrity")
  if not integrity:
    return {
        "status": "not-recorded",
        "detail":
            "the run record predates write-time artifact hashing; artifact integrity rests on the "
            "reconstruction and digest checks",
    }
  for rel in sorted(integrity):
    path = run_dir / rel
    if not path.is_file():
      raise errors.ReplayError(
          f"run bundle artifact {rel} is recorded in run.json but missing; the bundle changed after the run")
    if sha256_hex(path.read_bytes()) != integrity[rel]:
      raise errors.ReplayError(
          f"run bundle artifact {rel} no longer matches the hash recorded at run time; the bundle changed "
          "after the run")
  return {"status": "verified", "files": sorted(integrity)}


def _verify_source_snapshots(run_dir: Path, manifest: Manifest, verification: dict) -> None:
  """The bundle's evidence snapshots must match the frozen inputs' content hashes."""
  sources_dir = run_dir / "sources"
  if not sources_dir.is_dir():
    verification["source_snapshots"] = {
        "status": "not-written",
        "detail": "the run failed before writing its evidence snapshots",
    }
    return
  for source in manifest.sources:
    path = sources_dir / f"{source.ref}.md"
    if not path.is_file():
      raise errors.ReplayError(f"run bundle source snapshot {path.name} is missing; the bundle changed after the run")
    if sha256_hex(path.read_bytes()) != source.sha256:
      raise errors.ReplayError(
          f"run bundle source snapshot for {source.ref!r} no longer matches the frozen inputs' content hash; "
          "the bundle changed after the run")
  verification["source_snapshots"] = {"status": "verified", "count": len(manifest.sources)}


def _verify_prompt_fingerprints(record: dict, contract: ExchangeContract, verification: dict) -> None:
  """The recorded system prompts must match the prompts of the recorded contract."""
  current = {
      "editor": sha256_hex(contract.editor_system.encode("utf-8")),
      "reviewer": sha256_hex(contract.reviewer_system.encode("utf-8")),
  }
  recorded = record.get("system_prompts")
  if not recorded:
    verification["system_prompts"] = {
        "status": "not-recorded",
        "recorded": None,
        "match_current": None,
        "detail":
            "the run record predates system-prompt fingerprinting; the prompts that produced the "
            "recorded responses are not verifiable",
    }
    return
  mismatched = [role for role in sorted(current) if recorded.get(role) != current[role]]
  verification["system_prompts"] = {
      "status": "verified" if not mismatched else "mismatched",
      "recorded": recorded,
      "match_current": not mismatched,
  }
  if mismatched:
    raise errors.ReplayError(
        f"the recorded system prompt fingerprint(s) for {', '.join(mismatched)} do not match the "
        f"{contract.name} replay prompts; the recorded responses may not follow the contract this comparison "
        "validates against")


def _recorded_selections(record: dict, manifest: Manifest) -> dict[str, list[FeedbackSelection]]:
  """The feedback selection the run recorded, resolved against the frozen feedback pool."""
  by_event = {example.comment_event: example for example in manifest.feedback_examples}
  recorded_themes = {theme["name"]: theme for theme in record.get("themes", [])}
  selections: dict[str, list[FeedbackSelection]] = {}
  for theme in manifest.themes:
    recorded = recorded_themes.get(theme.name)
    if recorded is None:
      raise errors.ReplayError(
          f"the run record has no selection entry for theme {theme.name!r}; the record and the frozen inputs "
          "disagree")
    rows = []
    for item in recorded["selected_feedback"]:
      example = by_event.get(item["comment_event"])
      if example is None:
        raise errors.ReplayError(
            f"the recorded selection for theme {theme.name!r} names comment_event "
            f"{item['comment_event']!r}, which the frozen inputs do not carry")
      rows.append(
          FeedbackSelection(
              example=example,
              score=item["score"],
              matched_principles=list(item["matched_principles"]),
              matched_terms=list(item["matched_terms"])))
    selections[theme.name] = rows
  return selections


def _read_raw_if_present(run_dir: Path, name: str) -> str | None:
  path = run_dir / "raw" / name
  if not path.is_file():
    return None
  try:
    return path.read_text(encoding="utf-8")
  except OSError as e:
    raise errors.ReplayError(f"recorded raw file {path} is unreadable: {e}") from e


def _read_recorded_raw(run_dir: Path, rel: str, kind: str) -> str:
  path = run_dir / rel
  if not path.is_file():
    raise errors.ReplayError(f"the recorded {kind} {rel} is missing; the bundle changed after the run")
  try:
    return path.read_text(encoding="utf-8")
  except OSError as e:
    raise errors.ReplayError(f"recorded raw file {path} is unreadable: {e}") from e


def _read_stage_attempts(
    run_dir: Path,
    manifest: Manifest,
    record: dict,
    stage: str,
    contract: ExchangeContract,
) -> dict[str, list[StageAttempt]]:
  """The recorded attempt chain of one stage, per theme, requests and responses included.

  A completed run's bundle must be complete: every theme needs a chosen response. A failed
  run may legitimately stop mid-stage, and the gap becomes that arm's failure reason instead
  of a comparison error. A response without its request is a broken bundle either way.
  """
  if contract.bounded_recovery:
    return _read_attempt_chain(run_dir, manifest, record, stage)
  return _read_single_raw(run_dir, manifest, record, stage)


def _read_single_raw(run_dir: Path, manifest: Manifest, record: dict, stage: str) -> dict[str, list[StageAttempt]]:
  """The v2 layout: one fixed-name request/response pair per stage and theme, no attempt metadata."""
  attempts: dict[str, list[StageAttempt]] = {theme.name: [] for theme in manifest.themes}
  for theme in manifest.themes:
    name = theme.name
    request = _read_raw_if_present(run_dir, f"{stage}-{name}.request.txt")
    response = _read_raw_if_present(run_dir, f"{stage}-{name}.response.txt")
    if response is not None and request is None:
      raise errors.ReplayError(
          f"the run bundle at {run_dir} holds a recorded {stage} response for theme {name!r} without its "
          "request; the bundle changed after the run")
    if response is not None:
      attempts[name] = [
          StageAttempt(
              attempt=1, request=request, response=response, chosen=True, validation={"status": "not-recorded"})
      ]
      continue
    if record["status"] == "completed":
      raise errors.ReplayError(
          f"the completed run at {run_dir} is incomplete: no recorded {stage} response for theme {name!r}")
    if request is not None:
      attempts[name] = [
          StageAttempt(attempt=1, request=request, response=None, chosen=False, validation={"status": "not-recorded"})
      ]
  return attempts


def _read_attempt_chain(run_dir: Path, manifest: Manifest, record: dict, stage: str) -> dict[str, list[StageAttempt]]:
  """The v3 layout: the run record's calls, grouped per theme into ordered attempt chains."""
  attempts: dict[str, list[StageAttempt]] = {theme.name: [] for theme in manifest.themes}
  for call in record.get("calls", []):
    if call.get("role") != stage:
      continue
    name = call.get("theme")
    if name not in attempts:
      raise errors.ReplayError(f"the run record holds a {stage} call for unknown theme {name!r}")
    request_rel = call.get("request_file")
    if not request_rel:
      raise errors.ReplayError(
          f"the run record's {stage} call for theme {name!r} names no request file; the record changed "
          "after the run")
    request = _read_recorded_raw(run_dir, request_rel, f"{stage} request")
    response = None
    if call.get("response_file"):
      response = _read_recorded_raw(run_dir, call["response_file"], f"{stage} response")
    attempts[name].append(
        StageAttempt(
            attempt=int(call.get("attempt", 0)),
            request=request,
            response=response,
            chosen=bool(call.get("chosen")),
            validation=dict(call.get("validation") or {})))
  for rows in attempts.values():
    rows.sort(key=lambda a: a.attempt)
  if record["status"] == "completed":
    for theme in manifest.themes:
      if not any(a.chosen for a in attempts[theme.name]):
        raise errors.ReplayError(
            f"the completed run at {run_dir} is incomplete: no chosen {stage} response for theme "
            f"{theme.name!r}")
  return attempts


def _verify_stage_chain(
    *,
    contract: ExchangeContract,
    manifest: Manifest,
    selections: dict[str, list[FeedbackSelection]],
    role: str,
    stage_attempts: dict[str, list[StageAttempt]],
    chosen_outputs: dict[str, ThemeOutput | None],
    verification: dict,
) -> None:
  """Reconstruct and verify the recorded attempt chain of one stage, theme by theme.

  Attempt 1's request must be the request the recorded contract builds; a
  re-ask's request must be the bounded repair request over the same authorized
  evidence, the previous attempt's recorded raw response, and that attempt's
  recorded validation errors. Every non-chosen attempt must be recorded as a
  mechanical failure and its response must still fail validation now; the
  chosen attempt, if any, is the last one. For the reviewer, attempt 1's
  request is built from the chosen editor outputs, which is what proves the
  reviewer consumed exactly those responses.
  """
  verified = 0
  without: list[str] = []
  for theme in manifest.themes:
    name = theme.name
    attempts = stage_attempts.get(name, [])
    if not attempts:
      without.append(name)
      continue
    if contract.bounded_recovery:
      _verify_chain_structure(contract, role, manifest, theme, attempts, selections, chosen_outputs)
    base_request = _stage_base_request(contract, manifest, selections, chosen_outputs, role, theme)
    previous: StageAttempt | None = None
    for a in attempts:
      if a.attempt == 1:
        expected = base_request
      else:
        if previous is None or previous.response is None or not previous.validation.get("errors"):
          raise errors.ReplayError(
              f"the recorded {role} repair attempt {a.attempt} for theme {name!r} has no recoverable "
              "predecessor; the attempt chain is broken")
        expected = build_repair_request(base_request, previous.response, list(previous.validation["errors"]))
      if a.request != expected:
        _raise_request_mismatch(role, name, a.attempt)
      previous = a
    verified += 1
  verification[f"{role}_requests"] = _request_verification(verified, len(manifest.themes), without)


def _stage_base_request(
    contract: ExchangeContract,
    manifest: Manifest,
    selections: dict[str, list[FeedbackSelection]],
    chosen_outputs: dict[str, ThemeOutput | None],
    role: str,
    theme,
) -> str:
  """The base request of one stage for one theme, rebuilt under the recorded contract."""
  if role == "editor":
    return contract.build_editor_request(manifest, theme, selections[theme.name])
  editor_output = chosen_outputs.get(theme.name)
  if editor_output is None:
    raise errors.ReplayError(
        f"cannot establish what the reviewer of theme {theme.name!r} consumed: the recorded editor "
        "response does not parse or is missing")
  return contract.build_reviewer_request(manifest, theme, selections[theme.name], editor_output)


def _raise_request_mismatch(role: str, name: str, attempt: int) -> None:
  if attempt == 1 and role == "editor":
    raise errors.ReplayError(
        f"the recorded editor request for theme {name!r} does not match the request reconstructed "
        "from the frozen inputs and the recorded selection; the recorded editor response cannot be "
        "attributed to them")
  if attempt == 1 and role == "reviewer":
    raise errors.ReplayError(
        f"the recorded reviewer request for theme {name!r} does not match the request reconstructed "
        "from the recorded editor response; the reviewer did not verifiably consume that response, so the "
        "two alternatives cannot be established")
  raise errors.ReplayError(
      f"the recorded {role} repair request for theme {name!r} (attempt {attempt}) does not match the request "
      "reconstructed from the previous recorded response and its recorded validation errors; the attempt "
      "chain is broken")


def _verify_chain_structure(
    contract: ExchangeContract,
    role: str,
    manifest: Manifest,
    theme,
    attempts: list[StageAttempt],
    selections: dict[str, list[FeedbackSelection]],
    chosen_outputs: dict[str, ThemeOutput | None],
) -> None:
  """The chain must be a bounded recovery chain: consecutive, at most two, failures recorded."""
  name = theme.name
  numbers = [a.attempt for a in attempts]
  if numbers != list(range(1, len(numbers) + 1)):
    raise errors.ReplayError(f"the recorded {role} attempts for theme {name!r} are not a consecutive chain: {numbers}")
  if len(attempts) > MAX_STAGE_RESPONSES:
    raise errors.ReplayError(
        f"the recorded {role} attempts for theme {name!r} exceed the bounded recovery budget "
        f"({len(attempts)} responses > {MAX_STAGE_RESPONSES})")
  chosen = [a for a in attempts if a.chosen]
  if len(chosen) > 1:
    raise errors.ReplayError(f"the recorded {role} attempts for theme {name!r} mark {len(chosen)} responses as chosen")
  if chosen and chosen[0].attempt != attempts[-1].attempt:
    raise errors.ReplayError(
        f"the chosen {role} response for theme {name!r} is not the last recorded attempt; the attempt "
        "chain is broken")
  for a in attempts:
    if a.chosen:
      if a.validation.get("status") != "passed":
        raise errors.ReplayError(f"the chosen {role} attempt {a.attempt} for theme {name!r} is not recorded as passed")
      continue
    status = a.validation.get("status")
    if status == "transport-failed":
      if a.response is not None:
        raise errors.ReplayError(
            f"the {role} attempt {a.attempt} for theme {name!r} is recorded as transport-failed but "
            "carries a response")
    elif status == "failed":
      if not a.validation.get("errors"):
        raise errors.ReplayError(
            f"the failed {role} attempt {a.attempt} for theme {name!r} records no validation errors")
      if a.response is None:
        raise errors.ReplayError(f"the failed {role} attempt {a.attempt} for theme {name!r} has no recorded response")
      if _mechanical_error(a.response, role=f"{role}[{name}]", contract=contract, manifest=manifest, theme=theme,
                           selections=selections,
                           editor_output=chosen_outputs.get(name) if role == "reviewer" else None) is None:
        raise errors.ReplayError(
            f"the {role} attempt {a.attempt} for theme {name!r} is recorded as failed but its response "
            "passes mechanical validation; the attempt chain is inconsistent")
    else:
      raise errors.ReplayError(
          f"the non-chosen {role} attempt {a.attempt} for theme {name!r} has unknown validation status "
          f"{status!r}")


def _mechanical_error(
    raw: str,
    *,
    role: str,
    contract: ExchangeContract,
    manifest: Manifest,
    theme,
    selections: dict[str, list[FeedbackSelection]],
    editor_output: ThemeOutput | None,
) -> str | None:
  """The mechanical validation error of one response under the recorded contract, or None."""
  parse = contract.parse_reviewer_output if role.startswith("reviewer") else contract.parse_editor_output
  try:
    output = parse(raw, role=role)
  except errors.ReplayModelOutputError as e:
    return str(e)
  try:
    if role.startswith("reviewer"):
      contract.validate_reviewer(
          output,
          role=role,
          manifest=manifest,
          theme=theme,
          selections=selections[theme.name],
          editor_output=editor_output)
    else:
      contract.validate_editor(output, role=role, manifest=manifest, theme=theme, selections=selections[theme.name])
  except errors.ReplayValidationError as e:
    return str(e)
  return None


def _request_verification(verified: int, total: int, missing: list[str]) -> dict:
  if verified == total:
    return {"status": "verified", "themes_verified": verified, "themes_without_recorded_request": []}
  return {
      "status": "partial",
      "themes_verified": verified,
      "themes_without_recorded_request": sorted(missing),
      "detail": "the source run failed before this stage completed; its arm reports the failure",
  }


def _verification_limitations(verification: dict) -> list[str]:
  """What this run's record never saved, stated instead of silently certified."""
  limitations: list[str] = []
  if verification["artifacts"]["status"] == "not-recorded":
    limitations.append(
        "the run record predates write-time artifact hashing; raw files and snapshots are verified only "
        "by the reconstruction and digest checks")
  if verification["system_prompts"]["match_current"] is None:
    limitations.append(
        "the run record predates system-prompt fingerprinting; the prompts that produced the recorded "
        "responses are not verifiable")
  return limitations


# --- arms: parse, validate, finalize each alternative ---------------------------


def _parse_chosen_outputs(
    manifest: Manifest,
    stage_attempts: dict[str, list[StageAttempt]],
    *,
    role: str,
    contract: ExchangeContract,
    run_error: str | None = None,
) -> tuple[dict[str, ThemeOutput | None], dict[str, str]]:
  """Parse each theme's chosen response; themes without one become that arm's visible gap."""
  outputs: dict[str, ThemeOutput | None] = {}
  parse_errors: dict[str, str] = {}
  for theme in manifest.themes:
    name = theme.name
    attempts = stage_attempts.get(name, [])
    chosen = [a for a in attempts if a.chosen]
    if not chosen:
      outputs[name] = None
      parse_errors[name] = _gap_message(role, name, attempts, run_error)
      continue
    if chosen[0].response is None:
      raise errors.ReplayError(
          f"the chosen {role} response for theme {name!r} has no recorded response file; the bundle "
          "changed after the run")
    parse = contract.parse_reviewer_output if role == "reviewer" else contract.parse_editor_output
    try:
      outputs[name] = parse(chosen[0].response, role=f"{role}[{name}]")
    except errors.ReplayModelOutputError as e:
      outputs[name] = None
      parse_errors[name] = str(e)
  return outputs, parse_errors


def _gap_message(role: str, name: str, attempts: list[StageAttempt], run_error: str | None) -> str:
  suffix = f" (the run recorded: {run_error})" if run_error else ""
  if not attempts:
    return f"no recorded {role} response for theme {name!r}{suffix}"
  detail = "; ".join(f"attempt {a.attempt}: {a.validation.get('status') or 'unknown'}" for a in attempts)
  return f"no recorded {role} response for theme {name!r}{suffix} (recorded attempts: {detail})"


def _establish_arm(
    manifest: Manifest,
    outputs: dict[str, ThemeOutput | None],
    parse_errors: dict[str, str],
    *,
    role: str,
    contract: ExchangeContract,
    selections: dict[str, list[FeedbackSelection]],
    editor_outputs: dict[str, ThemeOutput | None],
) -> dict:
  """One arm's full final state, or a visible failure — never a stand-in for it."""
  for theme in manifest.themes:
    if theme.name in parse_errors:
      # Parse and validation errors already carry their "role[theme]" prefix; gaps name the theme.
      return _failed_arm(parse_errors[theme.name])
  ordered = [(theme, outputs[theme.name]) for theme in manifest.themes]
  try:
    for theme, output in ordered:
      if role == "reviewer":
        contract.validate_reviewer(
            output,
            role=f"{role}[{theme.name}]",
            manifest=manifest,
            theme=theme,
            selections=selections[theme.name],
            editor_output=editor_outputs.get(theme.name))
      else:
        contract.validate_editor(
            output, role=f"{role}[{theme.name}]", manifest=manifest, theme=theme, selections=selections[theme.name])
    base = {path: canonical_text(text) for path, text in manifest.base_entries().items()}
    result = finalize(base, ordered)
  except errors.ReplayValidationError as e:
    return _failed_arm(str(e))
  dispositions = _aggregate_candidate_results(ordered)
  themes_section = {}
  for theme, output in ordered:
    claimed = sorted({op.path for op in output.entries} | {path for row in output.candidates for path in row.paths})
    changed = [path for path in claimed if path in set(result.changed)]
    themes_section[theme.name] = {
        "changed_paths":
            changed,
        "entries":
            [
                {
                    "action": op.action,
                    "path": op.path,
                    "reason": op.reason,
                    "source_refs": list(op.source_refs),
                    "final_text": None if op.action in ("delete", "keep") else canonical_text(op.text or ""),
                } for op in output.entries
            ],
        "diffs": {
            path: _path_diff(base, result.final, path) for path in changed
        },
        "dispositions":
            [
                {
                    "source_ref": row.source_ref,
                    "outcome": row.outcome,
                    "paths": list(row.paths),
                    "reason": row.reason,
                } for row in output.candidates
            ],
    }
  return {
      "status": "established",
      "error": None,
      "reviewed_patch": result.patch,
      "changed_paths": result.changed,
      "content_digest": approval_digest(manifest.base_commit, result.patch),
      "counts":
          {
              "propose": sum(1 for row in dispositions if row["outcome"] == "propose"),
              "no_change": sum(1 for row in dispositions if row["outcome"] == "no_change"),
              "needs_decision": sum(1 for row in dispositions if row["outcome"] == "needs_decision"),
          },
      "dispositions": dispositions,
      "themes": themes_section,
  }


def _failed_arm(error: str) -> dict:
  return {
      "status": "failed",
      "error": error,
      "reviewed_patch": None,
      "changed_paths": None,
      "content_digest": None,
      "counts": None,
      "dispositions": None,
      "themes": None,
  }


def _path_diff(base: dict[str, str], final: dict[str, str], path: str) -> str:
  old = {path: base[path]} if path in base else {}
  new = {path: final[path]} if path in final else {}
  return build_patch(old, new)


def _verify_proposal(
    run_dir: Path,
    manifest: Manifest,
    record: dict,
    reviewer_arm: dict,
    selections: dict[str, list[FeedbackSelection]],
) -> dict:
  """A recorded proposal must still match the finalization of the recorded reviewer responses."""
  path = run_dir / "proposal.json"
  if not path.is_file():
    return {
        "status":
            "absent",
        "detail":
            "the source run wrote no proposal.json"
            if record["status"] == "failed" else "no proposal.json in a completed run's bundle",
    }
  try:
    proposal = json.loads(path.read_text(encoding="utf-8"))
  except (OSError, ValueError) as e:
    raise errors.ReplayError(f"the recorded proposal {path} is unreadable: {e}") from e
  if proposal.get("schema") != PROPOSAL_SCHEMA:
    raise errors.ReplayError(
        f"the recorded proposal declares schema {proposal.get('schema')!r}, expected {PROPOSAL_SCHEMA!r}")
  if proposal.get("base_commit") != manifest.base_commit:
    raise errors.ReplayError("the recorded proposal's base_commit does not match the frozen inputs")
  if proposal.get("approval_digest") != approval_digest(proposal.get("base_commit", ""), proposal.get("reviewed_patch",
                                                                                                      "")):
    raise errors.ReplayError(
        "the recorded proposal's approval digest does not match its own base_commit and reviewed_patch; "
        "the proposal changed after it was written")
  if {s["ref"]: s["sha256"] for s in proposal.get("sources", [])} != {s.ref: s.sha256 for s in manifest.sources}:
    raise errors.ReplayError("the recorded proposal's source list does not match the frozen inputs")
  if record.get("variant"):
    # Experimental contracts name exactly the feedback their view exposed; a tampered feedback
    # provenance list is a tampered record of what the stages saw.
    expected_refs = variants.contract_for_record(record).feedback_refs(manifest, selections)
    if proposal.get("feedback_refs") != expected_refs:
      raise errors.ReplayError(
          "the recorded proposal's feedback_refs do not match the feedback view the variant's stages "
          "saw; the proposal changed after the run")
  checked = ["base_commit", "approval_digest", "sources"]
  if reviewer_arm["status"] == "established":
    if proposal.get("reviewed_patch") != reviewer_arm["reviewed_patch"]:
      raise errors.ReplayError(
          "the recorded proposal's reviewed_patch does not match the finalization of the recorded reviewer "
          "responses; the proposal changed after the run")
    if canonical_bytes(proposal.get("candidate_results")) != canonical_bytes(reviewer_arm["dispositions"]):
      raise errors.ReplayError(
          "the recorded proposal's final dispositions do not match the recorded reviewer responses; the "
          "proposal changed after the run")
    checked += ["reviewed_patch", "candidate_results"]
    return {"status": "verified", "cross_checked": checked}
  return {
      "status": "partially-verified",
      "cross_checked": checked,
      "detail": "the post-review arm could not be re-established, so the proposal was checked only against itself",
  }


# --- comparison sections --------------------------------------------------------


def _new_provenance() -> dict:
  return {
      "shared_editor_response":
          {
              "statement":
                  "both arms parse and finalize the same recorded editor responses; each recorded reviewer "
                  "request is byte-identical to the request reconstructed from that response",
              "model_calls_made": 0,
              "editor_response_complete": None,
              "themes": {},
          }
  }


def _chosen_attempt(attempts: list[StageAttempt]) -> StageAttempt | None:
  for a in attempts:
    if a.chosen:
      return a
  return None


def _attempt_provenance(attempts: list[StageAttempt]) -> list[dict]:
  return [
      {
          "attempt": a.attempt,
          "chosen": a.chosen,
          "validation": a.validation.get("status"),
          "request_sha256": sha256_hex(a.request.encode("utf-8")) if a.request is not None else None,
          "response_sha256": sha256_hex(a.response.encode("utf-8")) if a.response is not None else None,
      } for a in attempts
  ]


def _fill_provenance(
    provenance: dict,
    manifest: Manifest,
    editor_stage: dict[str, list[StageAttempt]],
    reviewer_stage: dict[str, list[StageAttempt]],
) -> None:
  """Per theme: the hashes of the chosen responses both arms derive from, plus every attempt."""
  shared = provenance["shared_editor_response"]
  for theme in manifest.themes:
    name = theme.name
    editor_chosen = _chosen_attempt(editor_stage.get(name, []))
    reviewer_chosen = _chosen_attempt(reviewer_stage.get(name, []))
    shared["themes"][name] = {
        "editor_request_sha256": sha256_hex(editor_chosen.request.encode("utf-8")) if editor_chosen else None,
        "editor_response_sha256": sha256_hex(editor_chosen.response.encode("utf-8")) if editor_chosen else None,
        "reviewer_request_sha256": sha256_hex(reviewer_chosen.request.encode("utf-8")) if reviewer_chosen else None,
        "reviewer_response_sha256": sha256_hex(reviewer_chosen.response.encode("utf-8")) if reviewer_chosen else None,
        "attempts":
            {
                "editor": _attempt_provenance(editor_stage.get(name, [])),
                "reviewer": _attempt_provenance(reviewer_stage.get(name, [])),
            },
    }
  shared["editor_response_complete"] = all(
      _chosen_attempt(editor_stage.get(theme.name, [])) is not None for theme in manifest.themes)


def _source_run_section(run_dir: Path, record: dict, manifest: Manifest, contract: ExchangeContract) -> dict:
  return {
      "run_dir": str(run_dir),
      "status": record["status"],
      "error": record.get("error"),
      "created_at": record.get("created_at"),
      "mode": record["mode"],
      "input_identity": record.get("input_identity"),
      "base_commit": manifest.base_commit,
      "model": record.get("model"),
      "prompt_versions": record.get("prompt_versions"),
      "exchange_contract": contract.name,
      "manifest_path_recorded": record.get("manifest"),
  }


def _recovery_section(
    manifest: Manifest, editor_stage: dict[str, list[StageAttempt]], reviewer_stage: dict[str,
                                                                                          list[StageAttempt]]) -> dict:
  """What the bounded recovery did on this run, and what a recovery must not be read as."""
  recovered = []
  for stage_name, stage in (("editor", editor_stage), ("reviewer", reviewer_stage)):
    for theme in manifest.themes:
      if len(stage.get(theme.name, [])) > 1:
        recovered.append(f"{stage_name}[{theme.name}]")
  return {
      "policy":
          "at most one re-ask per stage response (two responses maximum); only mechanical validation "
          "failures re-ask — model judgments and transport failures never do",
      "note":
          "a recovered response replaces the failed one and can change an arm's judgment; that is stage "
          "execution/recovery, not independent-review quality gain",
      "themes_with_multiple_attempts": sorted(recovered),
  }


def _denominators(manifest: Manifest) -> dict:
  """Theme and candidate counts the frozen inputs fix; the comparison report adds its own note."""
  per_theme = {theme.name: len(theme.candidate_refs) for theme in manifest.themes}
  return {
      "themes": len(manifest.themes),
      "theme_names": [theme.name for theme in manifest.themes],
      "input_candidates": sum(per_theme.values()),
      "candidates_per_theme": per_theme,
  }


def _usage(record: dict) -> dict:
  """Usage exactly as the run recorded it, every recorded attempt included.

  Failed repair attempts and transport-failed calls carry their real usage when the endpoint
  reported it (unknown stays null), so the totals are the run's honest cost, not just the
  successful responses'.
  """
  calls = record.get("calls", [])
  per_call = [
      {
          "name": call.get("name"),
          "role": call.get("role"),
          "theme": call.get("theme"),
          "attempt": call.get("attempt"),
          "chosen": call.get("chosen"),
          "validation": (call.get("validation") or {}).get("status"),
          "latency_ms": call.get("latency_ms"),
          "prompt_tokens": call.get("prompt_tokens"),
          "output_tokens": call.get("output_tokens"),
          "cost_usd": call.get("cost_usd"),
      } for call in calls
  ]

  def role_rows(role: str) -> list[dict]:
    return [call for call in per_call if call["role"] == role]

  def total(rows: list[dict], key: str) -> int | None:
    values = [row[key] for row in rows]
    if not values or any(value is None for value in values):
      return None
    return sum(values)

  editor_rows = role_rows("editor")
  reviewer_rows = role_rows("reviewer")
  return {
      "editor":
          {
              "calls": len(editor_rows),
              "output_tokens": total(editor_rows, "output_tokens"),
              "latency_ms": total(editor_rows, "latency_ms"),
              "per_call": editor_rows,
          },
      "reviewer":
          {
              "calls": len(reviewer_rows),
              "output_tokens": total(reviewer_rows, "output_tokens"),
              "latency_ms": total(reviewer_rows, "latency_ms"),
              "per_call": reviewer_rows,
          },
      "incremental_review":
          {
              "calls": len(reviewer_rows),
              "output_tokens": total(reviewer_rows, "output_tokens"),
              "latency_ms": total(reviewer_rows, "latency_ms"),
              "note":
                  "the review calls only, failed recovery attempts included; the editor calls are "
                  "the shared base cost of both arms",
          },
  }


def _require_disjoint_comparison_output(output_dir: Path, run_dir: Path, cfg: CharlieBotConfig) -> None:
  """The comparison writes only into its own output root; the run bundle and the store are inputs."""
  out = _require_store_disjoint_output_root(output_dir, cfg, writer="comparison")
  run = run_dir.resolve()
  if out == run or out.is_relative_to(run) or run.is_relative_to(out):
    raise errors.ReplayIsolationError(
        f"output dir {output_dir} overlaps the source run directory {run_dir}; comparison never modifies "
        "its inputs")


# --- the comparison report ------------------------------------------------------


def render_comparison_report(comparison: dict) -> str:
  """Static HTML over the comparison dict; every dynamic value goes through ``html.escape``."""
  source = comparison["source_run"]
  arms = comparison["arms"]
  body = [
      "<h1>Memory replay &mdash; paired editor/reviewer comparison</h1>",
      f'<p class="muted">{_e(comparison["note"])}</p>',
      "<h2>Source run</h2>",
      "<table><tr><th>field</th><th>value</th></tr>" + "".join(
          _row(_e(key), f"<code>{_e(str(source[key]))}</code>") for key in (
              "run_dir", "status", "error", "created_at", "mode", "input_identity", "base_commit", "model",
              "prompt_versions", "exchange_contract", "manifest_path_recorded")) + "</table>",
      "<h2>Provenance &mdash; one recorded editor response</h2>",
      f'<p>{_e(comparison["provenance"]["shared_editor_response"]["statement"])} '
      f'Model calls made by this comparison: <code>{comparison["model_calls_made"]}</code>.</p>',
      _provenance_table(comparison["provenance"]["shared_editor_response"]),
      "<h2>Verification</h2>",
      _verification_section(comparison["verification"]),
      "<h2>Stage recovery</h2>",
      _recovery_section_html(comparison["recovery"]),
      "<h2>Denominators</h2>",
      _denominators_section(comparison["denominators"]),
      "<h2>Usage</h2>",
      _usage_section(comparison["usage"]),
      "<h2>Arms</h2>",
      _arm_section("Editor-only (from the recorded editor response)", arms["editor-only"]),
      _arm_section("Post-review (from the recorded reviewer responses)", arms["post-review"]),
      "<h2>Quality</h2>",
      '<div class="needs"><strong>Unjudged.</strong> '
      f'{_e(comparison["quality"]["note"])}</div>',
  ]
  return _page(
      "Memory replay — paired editor/reviewer comparison", 1150,
      [".failed{background:#ffebe9;border:1px solid #ff818266;border-radius:6px;padding:10px 14px;margin:8px 0}"], body)


def _provenance_table(shared: dict) -> str:
  rows = []
  for theme, hashes in sorted(shared["themes"].items()):
    rows.append(
        "<tr><td>{}</td>{}</tr>".format(
            _e(theme), "".join(
                f"<td><code>{_e(str(hashes[key]))}</code></td>" for key in (
                    "editor_request_sha256",
                    "editor_response_sha256",
                    "reviewer_request_sha256",
                    "reviewer_response_sha256",
                ))))
  attempts_note = ""
  attempt_counts = []
  for theme, hashes in sorted(shared["themes"].items()):
    for stage in ("editor", "reviewer"):
      count = len(hashes.get("attempts", {}).get(stage, []))
      if count > 1:
        attempt_counts.append(f"{stage}[{theme}]: {count} attempts")
  if attempt_counts:
    attempts_note = f'<p class="muted">recorded attempt chains: {_e("; ".join(attempt_counts))}</p>'
  return (
      "<table><tr><th>theme</th><th>editor request</th><th>editor response</th>"
      "<th>reviewer request</th><th>reviewer response</th></tr>" + "".join(rows) + "</table>"
      f'<p class="muted">editor responses complete: {shared["editor_response_complete"]}</p>' + attempts_note)


def _recovery_section_html(recovery: dict) -> str:
  parts = [
      f'<p>{_e(recovery["policy"])}</p>',
      f'<p class="muted">{_e(recovery["note"])}</p>',
  ]
  if recovery["themes_with_multiple_attempts"]:
    parts.append(
        "<p>themes with a recovered response: <code>" + _e(", ".join(recovery["themes_with_multiple_attempts"])) +
        "</code></p>")
  else:
    parts.append('<p class="muted">no stage needed a re-ask on this run.</p>')
  return "".join(parts)


def _verification_section(verification: dict) -> str:
  parts = ["<table><tr><th>check</th><th>result</th></tr>"]
  manifest_inputs = verification["manifest_inputs"]
  parts.append(
      _row(
          "frozen inputs", f"source <code>{_e(str(manifest_inputs['source']))}</code>, "
          f"identity verified <code>{_e(str(manifest_inputs['identity_verified']))}</code>, "
          f"external manifest <code>{_e(str(manifest_inputs['external_manifest'].get('state')))}</code>"))
  for key in ("artifacts", "source_snapshots", "system_prompts", "editor_requests", "reviewer_requests", "proposal"):
    parts.append(
        _row(
            _e(key), f"<code>{_e(str(verification[key].get('status')))}</code> "
            f"{_e(str(verification[key].get('detail', '')))}"))
  parts.append("</table>")
  limitations = verification.get("limitations") or []
  if limitations:
    parts.append(
        "<p><strong>Verification limits of this record:</strong></p><ul>" +
        "".join(f"<li>{_e(item)}</li>" for item in limitations) + "</ul>")
  return "".join(parts)


def _denominators_section(denominators: dict) -> str:
  rows = "".join(_row(_e(theme), count) for theme, count in sorted(denominators["candidates_per_theme"].items()))
  return (
      "<table><tr><th>theme</th><th>input candidates</th></tr>" + rows + "</table>"
      f'<p class="muted">themes <code>{denominators["themes"]}</code> · input candidates '
      f'<code>{denominators["input_candidates"]}</code> · {_e(denominators["note"])}</p>')


def _usage_section(usage: dict) -> str:
  parts = [
      "<table><tr><th>call</th><th>role</th><th>theme</th><th>attempt</th><th>outcome</th>"
      "<th>latency ms</th><th>output tokens</th></tr>"
  ]
  for call in usage["editor"]["per_call"] + usage["reviewer"]["per_call"]:
    parts.append(
        _row(
            _e(str(call["name"])), _e(str(call["role"])), _e(str(call["theme"])), _e(str(call["attempt"])),
            _e(str(call["validation"])), _e(str(call["latency_ms"])), _e(str(call["output_tokens"]))))
  parts.append("</table>")
  for role in ("editor", "reviewer", "incremental_review"):
    section = usage[role]
    parts.append(
        f'<p class="mono">{_e(role)}: calls {_e(str(section["calls"]))} · '
        f'output tokens {_e(str(section["output_tokens"]))} · latency ms {_e(str(section["latency_ms"]))}</p>')
  parts.append(f'<p class="muted">{_e(usage["incremental_review"]["note"])}</p>')
  return "".join(parts)


def _arm_section(title: str, arm: dict) -> str:
  parts = [f"<h3>{_e(title)}</h3>"]
  if arm["status"] == "failed":
    parts.append(
        f'<div class="failed"><strong>Arm failed &mdash; reported, not substituted.</strong> '
        f'{_e(str(arm["error"]))}</div>')
    return "".join(parts)
  parts.append(
      f'<p class="muted">changed paths <code>{_e(", ".join(arm["changed_paths"])) or "none"}</code> · '
      f'counts propose/no_change/needs_decision <code>{_e(str(arm["counts"]))}</code> · '
      f'content digest <code>{_e(str(arm["content_digest"])[:16])}&hellip;</code></p>')
  parts.append("<h4>Final diff</h4>")
  parts.append(f'<pre>{_e(arm["reviewed_patch"]) if arm["reviewed_patch"] else "(no changes)"}</pre>')
  for theme in sorted(arm["themes"]):
    section = arm["themes"][theme]
    parts.append(
        f"<details><summary>theme {_e(theme)} &mdash; changed: "
        f"{_e(', '.join(section['changed_paths'])) or 'none'}</summary>")
    parts.append(
        "<table><tr><th>source_ref</th><th>outcome</th><th>paths</th><th>reason</th></tr>" + "".join(
            _row(_e(row["source_ref"]), _e(row["outcome"]), _e(", ".join(row["paths"])), _e(row["reason"]))
            for row in section["dispositions"]) + "</table>")
    for path in section["changed_paths"]:
      parts.append(f"<p class='mono'>{_e(path)}</p><pre>{_e(section['diffs'][path])}</pre>")
      for entry in section["entries"]:
        if entry["path"] == path and entry["final_text"] is not None:
          parts.append(f"<details><summary>final text</summary><pre>{_e(entry['final_text'])}</pre></details>")
    parts.append("</details>")
  return "".join(parts)
