"""Replay runner: a frozen manifest in, a complete-entry proposal bundle out.

One run owns the whole pipeline for one manifest: load and validate the frozen
inputs, group them into themes, select the relevant prior user comments, call
the editor (and, in editor-review mode, the reviewer) over a content-only
transport, then let deterministic code validate the output, compute the final
diff against the frozen base, and write the proposal bundle. Bundle layout
under the requested output root::

    runs/<input-identity prefix>/
      proposal.json        # exactly the plan 4.1 schema
      report.html          # the review page
      sources/<ref>.md     # frozen evidence snapshots
      frozen/manifest.yaml # the fully inlined frozen inputs, self-contained
      run.json             # identity, model identity, selection, usage, timing, status,
                           # prompt fingerprints, and the bundle's write-time hashes
      raw/                 # the exact request and response text of every model call

Isolation is structural: the live memory store is never opened for reading or
writing — the manifest supplies the frozen store state — and the only writes
go to the requested output root, which must not overlap the store or any
frozen input. A completed run whose input identity matches (inputs, mode,
model, prompt versions) is reused without a model call; anything else runs
again. Model judgments never set the exit status; execution, parse, and
mechanical validation failures do.
"""

import json
import shutil
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

import structlog

from src.core.config import CharlieBotConfig, get_config, require_backend_option
from src.core.memory_replay import validate
from src.core.memory_replay.errors import (
    ReplayBackendError,
    ReplayError,
    ReplayIsolationError,
    ReplayModelOutputError,
)
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
from src.core.memory_replay.validate import theme_output_errors
from src.core.memory_replay.identity import approval_digest, input_identity, sha256_hex
from src.core.memory_replay.manifest import Manifest, Theme, dump_manifest, load_manifest
from src.core.memory_replay.report import ReportData, render_report
from src.core.memory_replay.retrieval import FeedbackSelection, select_feedback
from src.core.memory_replay.transport import (
    OpenAICompatibleTransport,
    ReplayTransport,
    TransportResult,
    request_model_for,
)

log = structlog.get_logger()

MODES = ("editor-only", "editor-review")
PROPOSAL_SCHEMA = "memory-replay-proposal/1"
RUN_SCHEMA = "memory-replay-run/1"
# The bounded recovery budget: a stage response that fails mechanical validation is re-asked
# at most once, so a stage consumes at most two model responses. Model judgments are never
# retried and transport/backend failures are never retried.
MAX_STAGE_RESPONSES = 2


@dataclass
class ReplayOptions:
  """The CLI-facing options: what to replay, where to write it, which model, which stages."""

  manifest: Path
  output_dir: Path
  backend: str
  mode: str  # one of MODES; the CLI narrows this with argparse choices


@dataclass
class ReplayOutcome:
  run_dir: Path
  proposal_path: Path
  report_path: Path
  reused: bool
  propose: int
  no_change: int
  needs_decision: int
  changed_paths: list[str]


def run_replay(
    options: ReplayOptions,
    *,
    cfg: CharlieBotConfig | None = None,
    transport_factory=None,
    now: datetime | None = None,
) -> ReplayOutcome:
  """Run one replay end to end; every failure is a :class:`ReplayError` with a visible message."""
  if options.mode not in MODES:
    raise ReplayError(f"unknown replay mode: {options.mode!r} (expected one of {', '.join(MODES)})")
  cfg = cfg or get_config()
  manifest = load_manifest(options.manifest)
  _require_disjoint_output_root(options.output_dir, options.manifest, manifest, cfg)
  option = _resolve_backend(cfg, options.backend)
  model_identity = {
      "backend": option.id,
      "backend_type": str(option.type),
      "model": request_model_for(option),
  }
  identity = input_identity(
      manifest=manifest,
      mode=options.mode,
      model_identity=model_identity,
      editor_prompt_version=EDITOR_PROMPT_VERSION,
      reviewer_prompt_version=REVIEWER_PROMPT_VERSION)

  runs_dir = options.output_dir / "runs"
  if (reused_dir := _find_completed_run(runs_dir, identity)) is not None:
    log.info("memory_replay_reused", run_dir=str(reused_dir))
    proposal = _read_json(reused_dir / "proposal.json")
    return _outcome(
        reused_dir,
        reused=True,
        candidate_results=proposal["candidate_results"],
        changed_paths=_changed_paths_from_patch(proposal["reviewed_patch"]))

  run_dir = runs_dir / identity[:16]
  if run_dir.exists():
    # Same identity, not completed: the leftover of a failed or killed run. Derived data, redone.
    shutil.rmtree(run_dir)
  run_dir.mkdir(parents=True)
  record: dict = {
      "schema": RUN_SCHEMA,
      "status": "running",
      "created_at": _timestamp(now),
      "mode": options.mode,
      "input_identity": identity,
      "prompt_versions": {
          "editor": EDITOR_PROMPT_VERSION,
          "reviewer": REVIEWER_PROMPT_VERSION
      },
      # Fingerprints of the exact system prompts sent to each stage, so a later comparison can
      # verify the recorded responses follow the contract it validates against.
      "system_prompts":
          {
              "editor": sha256_hex(EDITOR_SYSTEM.encode("utf-8")),
              "reviewer": sha256_hex(REVIEWER_SYSTEM.encode("utf-8")),
          },
      "model": model_identity,
      "manifest": str(options.manifest),
      # Filled by _write_frozen_inputs before the first model call; None only if that failed.
      "frozen_inputs": None,
      # sha256 of every bundle file at the moment the run wrote it (run.json and the derived
      # report excluded); a later comparison fails visibly when a recorded file changed.
      "bundle_integrity": {},
      # The bounded recovery policy this run ran under, recorded so a later comparison can
      # verify the attempt chain against the contract that produced it.
      "stage_recovery":
          {
              "max_responses_per_stage": MAX_STAGE_RESPONSES,
              "retrigger": "mechanical validation failure only; model judgments and transport "
                           "failures are never retried",
          },
      "themes": [],
      "unused_sources": manifest.unused_refs(),
      "calls": [],
      "editor_dispositions": [],
      "error": None,
  }
  try:
    _write_frozen_inputs(run_dir, manifest, record)
    transport = _build_transport(transport_factory, option)
    final_outputs, selections = _run_stages(options, manifest, transport, run_dir, record)
    base = {path: validate.canonical_text(text) for path, text in manifest.base_entries().items()}
    ordered = [(theme, final_outputs[theme.name]) for theme in manifest.themes]
    result = validate.finalize(base, ordered)
    candidate_results = _aggregate_candidate_results(ordered)
    _write_bundle(
        run_dir=run_dir,
        record=record,
        manifest=manifest,
        options=options,
        identity=identity,
        model_identity=model_identity,
        result=result,
        candidate_results=candidate_results,
        selections=selections,
    )
    record["status"] = "completed"
    _write_record(run_dir, record)
    log.info(
        "memory_replay_completed",
        run_dir=str(run_dir),
        propose=sum(1 for r in candidate_results if r["outcome"] == "propose"),
        changed=len(result.changed))
    return _outcome(run_dir, reused=False, candidate_results=candidate_results, changed_paths=result.changed)
  except Exception as e:
    record["status"] = "failed"
    record["error"] = str(e)
    _write_record(run_dir, record)
    log.error("memory_replay_failed", run_dir=str(run_dir), error=str(e))
    if isinstance(e, ReplayError):
      raise
    raise ReplayError(f"{type(e).__name__}: {e}") from e


def _run_stages(
    options: ReplayOptions,
    manifest: Manifest,
    transport: ReplayTransport,
    run_dir: Path,
    record: dict,
) -> tuple[dict[str, ThemeOutput], dict[str, list[FeedbackSelection]]]:
  """Editor for every theme, then (in editor-review mode) the reviewer over the same inputs.

  The editor request is built once per theme and shared verbatim by both modes,
  so the editor-only control sees exactly the input the editor-review run gave
  its editor. The reviewer sees that evidence plus the editor's proposed
  entries, never the editor's justifications. The returned outputs are the
  stage whose content wins: the reviewer's in editor-review mode, otherwise
  the editor's.
  """
  selections: dict[str, list[FeedbackSelection]] = {}
  editor_outputs: dict[str, ThemeOutput] = {}
  for theme in manifest.themes:
    selected = select_feedback(
        manifest.feedback_examples, principles=set(theme.principles), context_text=_theme_context_text(manifest, theme))
    selections[theme.name] = selected
    _record_theme(record, theme, selected)
    editor_outputs[theme.name] = _run_stage(
        transport=transport,
        role="editor",
        manifest=manifest,
        theme=theme,
        system=EDITOR_SYSTEM,
        build_request=lambda t=theme, s=selected: build_editor_request(manifest, t, s),
        run_dir=run_dir,
        record=record)
  for theme in manifest.themes:
    _record_editor_audit(record, theme.name, editor_outputs[theme.name])
  final_outputs = editor_outputs
  if options.mode == "editor-review":
    final_outputs = {}
    for theme in manifest.themes:
      final_outputs[theme.name] = _run_stage(
          transport=transport,
          role="reviewer",
          manifest=manifest,
          theme=theme,
          system=REVIEWER_SYSTEM,
          build_request=lambda t=theme: build_reviewer_request(manifest, t, selections[t.name], editor_outputs[t.name]),
          run_dir=run_dir,
          record=record)
  return final_outputs, selections


def _record_editor_audit(record: dict, theme_name: str, output: ThemeOutput) -> None:
  """The editor's own rows and reasons, kept for audit; the reviewer never sees them."""
  role = f"editor[{theme_name}]"
  for op in output.entries:
    record["editor_dispositions"].append(
        {
            "role": role,
            "kind": "entry",
            "name": op.path,
            "detail": f"action {op.action}; reason {op.reason}",
        })
  for row in output.candidates:
    record["editor_dispositions"].append(
        {
            "role": role,
            "kind": "candidate",
            "name": row.source_ref,
            "detail": f"outcome {row.outcome}; reason {row.reason}",
        })


def _run_stage(
    *,
    transport: ReplayTransport,
    role: str,
    manifest: Manifest,
    theme: Theme,
    system: str,
    build_request,
    run_dir: Path,
    record: dict,
) -> ThemeOutput:
  """One stage for one theme: up to two model responses, the second only a bounded repair.

  The first response is validated as sent; on a mechanical failure (JSON shape,
  unknown ref, path, format, or patch/disposition inconsistency) the same stage
  is re-asked once with the original authorized evidence, its own previous raw
  response, and the concrete validation errors. Model judgments are never retry
  triggers and transport or backend failures are never retried — they fail the
  run immediately after being recorded. A second invalid response fails
  visibly; nothing is coerced into a valid output.
  """
  base_request = build_request()
  previous: tuple[str, list[str]] | None = None  # (raw response, its mechanical errors)
  for attempt in range(1, MAX_STAGE_RESPONSES + 1):
    user = base_request if previous is None else build_repair_request(base_request, previous[0], previous[1])
    result, call_entry = _call_attempt(
        transport=transport, role=role, theme_name=theme.name, attempt=attempt, system=system, user=user,
        run_dir=run_dir, record=record)
    output, errors = _parse_and_validate(result.text, role=role, manifest=manifest, theme=theme)
    call_entry["chosen"] = not errors
    call_entry["validation"] = {"status": "passed"} if not errors else {"status": "failed", "errors": errors}
    _write_record(run_dir, record)
    if not errors:
      return output
    previous = (result.text, errors)
  raise ReplayError(
      f"{role}[{theme.name}]: stage response failed mechanical validation after 1 re-ask "
      f"({MAX_STAGE_RESPONSES} responses); errors of the last response: {previous[1][-1]}")


def _call_attempt(
    *,
    transport: ReplayTransport,
    role: str,
    theme_name: str,
    attempt: int,
    system: str,
    user: str,
    run_dir: Path,
    record: dict,
) -> tuple[TransportResult, dict]:
  """One model call of one stage's attempt chain: persist request/response/usage, return the entry.

  The call entry is appended before the transport call and completed after it, so a transport
  failure is recorded with the request, a null response, and unknown usage — never silently
  dropped and never retried. The caller fills in the validation outcome and the chosen flag.
  """
  name = f"{role}-{theme_name}.attempt-{attempt}"
  raw_dir = run_dir / "raw"
  raw_dir.mkdir(exist_ok=True)
  request_rel = f"raw/{name}.request.txt"
  (run_dir / request_rel).write_text(user, encoding="utf-8")
  _record_artifact_hash(run_dir, record, request_rel)
  entry: dict = {
      "name": name,
      "role": role,
      "theme": theme_name,
      "attempt": attempt,
      "request_file": request_rel,
      "response_file": None,
      "validation": {"status": "transport-failed"},
      "chosen": False,
      "latency_ms": None,
      "prompt_tokens": None,
      "output_tokens": None,
      # Known only when an endpoint reports pricing; replay never guesses a rate.
      "cost_usd": None,
  }
  record["calls"].append(entry)
  try:
    result = transport.complete(system=system, user=user)
  except Exception as e:
    entry["validation"] = {"status": "transport-failed", "error": str(e)}
    _write_record(run_dir, record)
    raise
  response_rel = f"raw/{name}.response.txt"
  (run_dir / response_rel).write_text(result.text, encoding="utf-8")
  _record_artifact_hash(run_dir, record, response_rel)
  entry["response_file"] = response_rel
  entry["latency_ms"] = result.latency_ms
  entry["prompt_tokens"] = result.prompt_tokens
  entry["output_tokens"] = result.output_tokens
  return result, entry


def _parse_and_validate(raw: str, *, role: str, manifest: Manifest, theme: Theme) -> tuple[ThemeOutput | None, list[str]]:
  """The mechanical gate of one stage response: parse, then validate, collecting every error."""
  role_ctx = f"{role}[{theme.name}]"
  try:
    output = parse_model_output(raw, role=role_ctx)
  except ReplayModelOutputError as e:
    return None, [str(e)]
  errors = theme_output_errors(output, role=role_ctx, manifest=manifest, theme=theme, allow_no_write_citations=True)
  return (None, errors) if errors else (output, [])


def _aggregate_candidate_results(ordered: list[tuple[Theme, ThemeOutput]]) -> list[dict]:
  rows: list[dict] = []
  for _, output in ordered:
    rows.extend(
        {
            "source_ref": row.source_ref,
            "outcome": row.outcome,
            "paths": list(row.paths),
            "reason": row.reason
        } for row in output.candidates)
  return sorted(rows, key=lambda row: row["source_ref"])


def _write_frozen_inputs(run_dir: Path, manifest: Manifest, record: dict) -> None:
  """Persist the resolved frozen inputs inside the run bundle before any model call.

  The bundle then carries everything a later comparison needs even after the
  original manifest moves or changes: the fully inlined manifest plus every
  source snapshot, each hashed into ``bundle_integrity`` at write time.
  """
  frozen_dir = run_dir / "frozen"
  frozen_dir.mkdir(parents=True)
  (frozen_dir / "manifest.yaml").write_text(dump_manifest(manifest), encoding="utf-8")
  sources_dir = run_dir / "sources"
  sources_dir.mkdir(exist_ok=True)
  for source in manifest.sources:
    (sources_dir / f"{source.ref}.md").write_text(source.text, encoding="utf-8")
  record["frozen_inputs"] = {"manifest": "frozen/manifest.yaml"}
  for rel in ["frozen/manifest.yaml", *(f"sources/{source.ref}.md" for source in manifest.sources)]:
    _record_artifact_hash(run_dir, record, rel)


def _record_artifact_hash(run_dir: Path, record: dict, relpath: str) -> str:
  """Hash one written bundle file into run.json's integrity map; the comparison rechecks it."""
  digest = sha256_hex((run_dir / relpath).read_bytes())
  record["bundle_integrity"][relpath] = digest
  return digest


def _write_bundle(
    *,
    run_dir: Path,
    record: dict,
    manifest: Manifest,
    options: ReplayOptions,
    identity: str,
    model_identity: dict,
    result: validate.FinalResult,
    candidate_results: list[dict],
    selections: dict[str, list[FeedbackSelection]],
) -> None:
  """Write the proposal, the report, and the run record.

  The frozen inputs (manifest copy and source snapshots) were written before the
  first model call, so failed runs are self-contained too; this only adds the
  derived proposal.
  """
  selected_events = sorted(
      {selection.example.comment_event for selected in selections.values() for selection in selected})
  feedback_refs = []
  for event in selected_events:
    example = next(f for f in manifest.feedback_examples if f.comment_event == event)
    feedback_refs.append(
        {
            "comment_event": event,
            "approved_change_ref": example.approved_change.approved_change_ref if example.approved_change else None,
        })
  proposal = {
      "schema": PROPOSAL_SCHEMA,
      "base_commit": manifest.base_commit,
      "sources": [{
          "ref": s.ref,
          "sha256": s.sha256,
          "snapshot": f"sources/{s.ref}.md"
      } for s in manifest.sources],
      "feedback_refs": feedback_refs,
      "candidate_results": candidate_results,
      "reviewed_patch": result.patch,
      "approval_digest": approval_digest(manifest.base_commit, result.patch),
  }
  (run_dir / "proposal.json").write_text(
      json.dumps(proposal, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
  _record_artifact_hash(run_dir, record, "proposal.json")
  changed_mapping = [
      {
          "path":
              path,
          "dispositions":
              [
                  {
                      "source_ref": row["source_ref"],
                      "outcome": row["outcome"]
                  } for row in candidate_results if path in row["paths"]
              ],
      } for path in result.changed
  ]
  report = render_report(
      ReportData(
          mode=options.mode,
          created_at=record["created_at"],
          base_commit=manifest.base_commit,
          input_identity=identity,
          model_identity=model_identity,
          prompt_versions=record["prompt_versions"],
          patch=result.patch,
          candidate_results=candidate_results,
          changed_mapping=changed_mapping,
          sources=manifest.sources,
          selections=selections,
          editor_dispositions=record["editor_dispositions"],
          calls=record["calls"],
          unused_sources=record["unused_sources"],
      ))
  (run_dir / "report.html").write_text(report, encoding="utf-8")
  _write_record(run_dir, record)


def _record_theme(record: dict, theme: Theme, selected: list[FeedbackSelection]) -> None:
  record["themes"].append(
      {
          "name":
              theme.name,
          "principles":
              list(theme.principles),
          "candidate_refs":
              list(theme.candidate_refs),
          "entry_refs":
              list(theme.entry_refs),
          "document_refs":
              list(theme.document_refs),
          "selected_feedback":
              [
                  {
                      "comment_event": s.example.comment_event,
                      "score": s.score,
                      "matched_principles": s.matched_principles,
                      "matched_terms": s.matched_terms,
                  } for s in selected
              ],
      })


def _require_disjoint_output_root(
    output_dir: Path, manifest_path: Path, manifest: Manifest, cfg: CharlieBotConfig) -> None:
  """Reject any output root that overlaps the live memory store or a frozen input file."""
  out = output_dir.resolve()
  if out.exists() and not out.is_dir():
    raise ReplayIsolationError(
        f"output dir {output_dir} exists and is not a directory; replay writes only to an isolated output root")
  store = cfg.memory_dir.resolve()
  if out == store or out.is_relative_to(store) or store.is_relative_to(out):
    raise ReplayIsolationError(
        f"output dir {output_dir} overlaps the live memory store {cfg.memory_dir}; replay writes only to an "
        "isolated output root")
  for frozen_path in [manifest_path, *manifest.frozen_files()]:
    resolved = frozen_path.resolve()
    if out == resolved or out.is_relative_to(resolved) or resolved.is_relative_to(out):
      raise ReplayIsolationError(
          f"output dir {output_dir} overlaps frozen input {frozen_path}; replay writes only to an isolated "
          "output root")


def _resolve_backend(cfg: CharlieBotConfig, backend_id: str):
  try:
    return require_backend_option(cfg, backend_id, subject="replay ")
  except ValueError as e:
    raise ReplayBackendError(str(e)) from e


def _build_transport(transport_factory, option):
  if transport_factory is not None:
    return transport_factory()
  return OpenAICompatibleTransport.from_config(option)


def _find_completed_run(runs_dir: Path, identity: str) -> Path | None:
  """The completed run with this exact input identity, or None; unreadable records are skipped loudly."""
  if not runs_dir.is_dir():
    return None
  for record_path in sorted(runs_dir.glob("*/run.json")):
    try:
      record = json.loads(record_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
      log.warning("memory_replay_run_record_unreadable", path=str(record_path), error=str(e))
      continue
    if record.get("input_identity") == identity and record.get("status") == "completed":
      return record_path.parent
  return None


def _read_json(path: Path) -> dict:
  try:
    return json.loads(path.read_text(encoding="utf-8"))
  except (OSError, ValueError) as e:
    raise ReplayError(f"completed run at {path.parent} has an unreadable {path.name}: {e}") from e


def _changed_paths_from_patch(patch: str) -> list[str]:
  return sorted({line[len("--- a/"):] for line in patch.split("\n") if line.startswith("--- a/")})


def _outcome(run_dir: Path, *, reused: bool, candidate_results: list[dict], changed_paths: list[str]) -> ReplayOutcome:
  return ReplayOutcome(
      run_dir=run_dir,
      proposal_path=run_dir / "proposal.json",
      report_path=run_dir / "report.html",
      reused=reused,
      propose=sum(1 for r in candidate_results if r["outcome"] == "propose"),
      no_change=sum(1 for r in candidate_results if r["outcome"] == "no_change"),
      needs_decision=sum(1 for r in candidate_results if r["outcome"] == "needs_decision"),
      changed_paths=changed_paths,
  )


def _write_record(run_dir: Path, record: dict) -> None:
  (run_dir / "run.json").write_text(
      json.dumps(record, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def _timestamp(now: datetime | None) -> str:
  return (now or datetime.now(UTC)).strftime("%Y-%m-%dT%H:%M:%SZ")


def _theme_context_text(manifest: Manifest, theme: Theme) -> str:
  parts = [theme.name]
  for kind in ("candidate", "entry", "document"):
    parts.extend(source.text for source in manifest.theme_sources(theme, kind))
  return "\n".join(parts)
