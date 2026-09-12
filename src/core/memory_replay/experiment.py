"""The fixed-input variant experiment: five named curation variants over identical frozen cases.

``charliebot memory experiment`` is the one runnable command behind the approved redesign's
evidence step. One invocation runs every selected variant against every supplied manifest case
under a shared experimental control — identical frozen inputs, one content-only transport and
backend, the same JSON evidence, bounded recovery, validation, proposal finalization, and
paired-comparison machinery — and writes a self-contained JSON summary plus a readable HTML
report that link the exact replay bundles and paired comparisons.

Per case and variant the command records one editor-review replay run (the variant's contract
decides the feedback view, the rationale visibility, and the reviewer capability) and derives
the no-second-review control with the existing paired comparison, from the exact editor
response that variant's reviewer consumed — never from a fresh editor call.

Honesty rules this module enforces structurally:

- a failed arm is recorded with its full attempt chain and usage and is never substituted with
  ``no_change`` output, and it is never silently deleted and rerun under the same output root —
  a fresh output root is the intentional new experimental draw;
- before any replay can delete or replace evidence, the experiment validates the existing run
  records and the expected run-directory occupancy: a corrupted, missing, or unreadable record,
  a record whose input identity disagrees with its own directory, or an orphaned directory with
  partial attempt evidence blocks the arm as a visible failed condition that keeps every file
  byte-for-byte — with no model call and no silent repair under the same output root;
- repeating the experiment reuses completed bundles without model calls and preserves the
  recorded failures; a record of a different, legitimate input identity coexists untouched in
  its own run directory;
- every variant's editor is called for itself; the summary records per-case, per-theme call
  provenance (run-record reference, chosen attempt and response reference, content digest) and
  labels byte-identical recorded content across variants as identical content — never as shared
  sampling or a reused call, and equal text saves no calls or cost;
- semantic quality stays unjudged: fewer lines, fewer proposals, or more deletions are data,
  never an automatic pass, and the exit code reports execution and format success only.
"""

import json
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

import structlog

from src.core.config import CharlieBotConfig, get_config
from src.core.memory_replay import variants
from src.core.memory_replay.compare import CompareOptions, _denominators, _usage, run_comparison
from src.core.memory_replay.errors import ReplayError
from src.core.memory_replay.identity import sha256_hex
from src.core.memory_replay.manifest import REF_RE, Manifest, load_manifest
from src.core.memory_replay.report import _e, _page
from src.core.memory_replay.runner import (
    ReplayOptions,
    _require_disjoint_output_root,
    _timestamp,
    compute_input_identity,
    resolve_backend_identity,
    run_directory_name,
    run_replay,
)
from src.core.memory_replay.validate import apply_unified_patch, canonical_text

log = structlog.get_logger()

# Summary schema v2: per-case/theme editor call provenance replaces the old content-hash "draw"
# grouping, and blocked-evidence arms joined the arm statuses. v1 summaries are not reinterpreted.
EXPERIMENT_SCHEMA = "memory-curation-variant-experiment/2"
MODE = "editor-review"

QUALITY_NOTE = (
    "No semantic quality judgment is made here: fewer lines, fewer proposals, or more deletions never "
    "establish better curation. An independent assessment supplies quality; this experiment supplies "
    "execution evidence.")
EDITOR_CALLS_NOTE = (
    "Every variant makes its own editor calls; the engine never feeds one variant's recorded response to "
    "another variant's editor. Byte-identical recorded content across variants is labeled identical "
    "content — content equality is not shared sampling or a reused call, and it saves nothing: usage is "
    "the sum of actual attempts, so equal text costs the same as different text. Each variant's "
    "editor-only control is still the exact recorded response its own reviewer consumed.")
FEEDBACK_VIEW_NOTE = (
    "Old-flow feedback is not no-feedback: the baseline's raw-history view exposes the manifest's whole "
    "prior-comment pool — comment texts with provenance ids, as the production selector's user-message "
    "digest did. The approved-edit-feedback intervention narrows that to the relevance-selected "
    "structured comments with approved before/after examples. Both views render from the identical "
    "frozen pool; an absent approved after-text stays absent, and an approved creation or deletion "
    "keeps its empty side verbatim.")
CONTROL_NOTE = (
    "The editor-only arm of every variant is the existing paired comparison's control, derived from the "
    "exact recorded editor response that variant's reviewer consumed — never from a fresh editor call.")

COMMON_ADAPTATIONS = (
    "Frozen pipeline upstream: mining, daily scheduling, the pending-proposal guard, and the production "
    "lint/report subprocesses are frozen out. Every candidate arrives as a manifest source; nothing is "
    "mined, scheduled, read from, or written to the live memory store, and the only writes go to the "
    "experiment output root.",
    "Frozen evidence reads: the production prompts read the llm-context-guideline skill and the master "
    "prompt's Writing Style section from disk; the experiment supplies the admission guideline as a "
    "frozen manifest source instead, and every variant's prompts state the user's explicit "
    "English-memory requirement so the guideline's stale language clause never confounds a comparison.",
    "JSON operations replace working-tree edits: the selector's in-place entry edits and the reviewer's "
    "git-checkout restorations become base-relative entry operations over the frozen base; the unified "
    "diff, its round-trip check, and the final dispositions come from the deterministic machinery every "
    "variant shares.",
    "The handoff sheet becomes recorded data: the selector's per-candidate dispositions and its three "
    "Action/Home/Brevity proof lines are model output kept in the run audit; whether the reviewer "
    "request carries them is the declared rationale-visibility dimension, never a re-rendering written "
    "by code.",
    "Trim-only review is validated mechanically as line removal: a trim-only reviewer's returned text "
    "for a path must be the selector's proposed text for that path with whole lines removed, in order — "
    "the narrow mechanical form of the pinned reviewer's 'deleted or trimmed' under its no-new-prose "
    "rule. Within-line rewriting is unavailable to it; a capability violation is a visible execution "
    "failure eligible for the same one bounded re-ask, never coerced into acceptance.",
    "One bounded mechanical re-ask per stage response; the same content-only transport and backend, "
    "format recovery, validation, proposal finalization, and paired comparison for every variant. The "
    "frozen input bundle is identical across variants; only the declared dimensions differ.",
    "Citations are validated against what each stage actually saw — its theme's assigned sources, the "
    "guideline, and exactly the feedback ids its view exposed — on the initial attempt, on repairs, and "
    "in the later comparison; a response cannot cite evidence its request never carried.",
    "Scope: this measures curation judgment behavior on frozen inputs. It does not reproduce the old "
    "tool-using runtime end to end, delivers no semantic-quality verdict, and switches nothing in "
    "production.",
)


@dataclass
class ExperimentOptions:
  """The CLI-facing options: which frozen cases, which variants, where to write, which backend."""

  manifests: list[Path]
  output_dir: Path
  backend: str
  variants: list[str] | None = None  # None = the full defined set, in canonical order


@dataclass
class ExperimentOutcome:
  summary_path: Path
  report_path: Path
  failed_arms: list[str] = field(default_factory=list)


def run_experiment(
    options: ExperimentOptions,
    *,
    cfg: CharlieBotConfig | None = None,
    transport_factory=None,
    now: datetime | None = None,
) -> ExperimentOutcome:
  """Run the selected variants over every supplied case; every failure is a :class:`ReplayError`.

  A failed arm is recorded and the remaining arms still run; the summary is always written when
  the inputs themselves are valid. :attr:`ExperimentOutcome.failed_arms` reports which arms
  failed execution so the CLI can surface them.
  """
  cfg = cfg or get_config()
  if not options.manifests:
    raise ReplayError("the experiment needs at least one input manifest")
  contracts = _resolve_contracts(options.variants)
  option, model_identity = resolve_backend_identity(cfg, options.backend)
  del option  # each replay run resolves its own transport from the same configuration
  cases = _load_cases(options.manifests, options.output_dir, cfg)

  arms: list[dict] = []
  for case_id, manifest_path, manifest in cases:
    for contract in contracts:
      arms.append(
          _run_arm(
              case_id=case_id,
              manifest_path=manifest_path,
              manifest=manifest,
              contract=contract,
              options=options,
              cfg=cfg,
              model_identity=model_identity,
              transport_factory=transport_factory))

  summary = _build_summary(cases=cases, contracts=contracts, model_identity=model_identity, arms=arms, now=now)
  options.output_dir.mkdir(parents=True, exist_ok=True)
  summary_path = options.output_dir / "experiment.json"
  report_path = options.output_dir / "report.html"
  summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
  report_path.write_text(render_experiment_report(summary), encoding="utf-8")
  failed_arms = [
      f"{arm['case']}/{arm['variant']}" for arm in arms
      if arm["run"]["status"] != "completed" or arm["comparison"]["status"] in ("failed", "blocked")
  ]
  log.info("memory_experiment_completed", output_dir=str(options.output_dir), arms=len(arms), failed=len(failed_arms))
  return ExperimentOutcome(summary_path=summary_path, report_path=report_path, failed_arms=failed_arms)


def _resolve_contracts(selected: list[str] | None) -> list[variants.ExperimentContract]:
  if not selected:
    return [variants.resolve_variant(name) for name in variants.VARIANT_ORDER]
  contracts: list[variants.ExperimentContract] = []
  seen: set[str] = set()
  for name in selected:
    if name in seen:
      raise ReplayError(f"variant {name!r} is selected more than once")
    seen.add(name)
    contracts.append(variants.resolve_variant(name))
  return contracts


def _load_cases(manifests: list[Path], output_dir: Path, cfg: CharlieBotConfig) -> list[tuple[str, Path, Manifest]]:
  cases: list[tuple[str, Path, Manifest]] = []
  by_id: dict[str, Path] = {}
  for manifest_path in manifests:
    manifest = load_manifest(manifest_path)
    case_id = manifest_path.stem
    if REF_RE.match(case_id) is None:
      raise ReplayError(
          f"manifest {manifest_path} has case id {case_id!r}, which is not a usable directory name "
          "(use manifest file names of letters, digits, dots, underscores, and dashes)")
    if case_id in by_id:
      raise ReplayError(f"two input manifests share the case id {case_id!r} ({by_id[case_id]} and {manifest_path})")
    by_id[case_id] = manifest_path
    _require_disjoint_output_root(output_dir, manifest_path, manifest, cfg)
    cases.append((case_id, manifest_path, manifest))
  return cases


def _run_arm(
    *,
    case_id: str,
    manifest_path: Path,
    manifest: Manifest,
    contract: variants.ExperimentContract,
    options: ExperimentOptions,
    cfg: CharlieBotConfig,
    model_identity: dict,
    transport_factory,
) -> dict:
  """One case x variant: validate existing evidence, reuse, run, or preserve — then the comparison."""
  identity = compute_input_identity(manifest, mode=MODE, model_identity=model_identity, contract=contract)
  variant_runs_root = options.output_dir / "cases" / case_id / "runs" / contract.name
  runs_dir = variant_runs_root / "runs"
  existing = _scan_existing_runs(runs_dir, identity)
  if existing.blocked:
    return _blocked_arm(
        case_id=case_id,
        manifest_path=manifest_path,
        manifest=manifest,
        contract=contract,
        identity=identity,
        blocked=existing.blocked,
        preserved=existing.preserved,
        options=options)

  run_error: str | None = None
  run_dir: Path | None = existing.completed or (existing.preserved[-1] if existing.preserved else None)
  reused = existing.completed is not None
  if existing.completed is None and not existing.preserved:
    try:
      outcome = run_replay(
          ReplayOptions(manifest=manifest_path, output_dir=variant_runs_root, backend=options.backend, mode=MODE),
          cfg=cfg,
          transport_factory=transport_factory,
          contract=contract)
      run_dir = outcome.run_dir
      reused = outcome.reused
    except ReplayError as e:
      run_error = str(e)
      # run_replay records the failure in its bundle before raising; keep that bundle as-is.
      existing = _scan_existing_runs(runs_dir, identity)
      if existing.blocked:
        return _blocked_arm(
            case_id=case_id,
            manifest_path=manifest_path,
            manifest=manifest,
            contract=contract,
            identity=identity,
            blocked=existing.blocked,
            preserved=existing.preserved,
            options=options,
            run_error=run_error)
      run_dir = existing.completed or (existing.preserved[-1] if existing.preserved else None)

  record: dict | None = None
  if run_dir is not None:
    record = _read_run_record(run_dir)
  if record is None:
    run_status, run_status_error = "failed", run_error
  else:
    run_status = record.get("status") or "unknown"
    run_status_error = run_error or record.get("error")

  comparison = _run_comparison_for(run_dir, case_id, contract, options, cfg)

  arm = {
      "case":
          case_id,
      "manifest":
          str(manifest_path),
      "variant":
          contract.name,
      "variant_title":
          contract.title,
      "input_identity":
          identity,
      "base_commit":
          manifest.base_commit,
      "prompt_versions":
          record.get("prompt_versions") if record else {
              "editor": contract.editor_prompt_version,
              "reviewer": contract.reviewer_prompt_version,
          },
      "denominators":
          _denominators(manifest),
      "run":
          _run_section(run_dir, record, run_status, run_status_error, reused, existing.preserved, options.output_dir),
      "comparison":
          comparison,
      "editor_provenance":
          _editor_provenance(run_dir, record, manifest, options.output_dir),
  }
  return arm


@dataclass
class _ExistingRuns:
  """The classified contents of one variant's runs directory, before any replay may run.

  ``completed`` and ``preserved`` are bundles recorded under this exact input identity;
  ``blocked`` lists uninterpretable evidence with the reason it blocks the arm.
  """

  completed: Path | None
  preserved: list[Path]
  blocked: list[tuple[Path, str]]


def _scan_existing_runs(runs_dir: Path, identity: str) -> _ExistingRuns:
  """Classify every run directory under one variant's runs root before any replay may run.

  A completed bundle recorded under this exact identity is reused without model calls; a failed
  one stays preserved as recorded. Records of different, legitimate input identities (earlier
  frozen inputs of the same case) coexist untouched in their own directories. Anything that
  cannot be interpreted — a missing or unreadable run record, a record whose input identity
  disagrees with its own directory name, or an orphaned directory without a readable record —
  blocks the arm: it neither runs nor reuses, every file stays byte-for-byte, and a fresh
  output root is the intentional way to request a new draw.
  """
  result = _ExistingRuns(completed=None, preserved=[], blocked=[])
  if not runs_dir.is_dir():
    return result
  for run_dir in sorted(path for path in runs_dir.iterdir() if path.is_dir()):
    record_path = run_dir / "run.json"
    if not record_path.is_file():
      result.blocked.append((run_dir, "run record run.json is missing from an existing run directory"))
      continue
    try:
      record = json.loads(record_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
      result.blocked.append((run_dir, f"run record run.json is unreadable: {e}"))
      continue
    recorded = record.get("input_identity")
    if not isinstance(recorded, str) or not recorded:
      result.blocked.append((run_dir, "run record carries no input_identity"))
      continue
    if run_dir.name != run_directory_name(recorded):
      result.blocked.append(
          (
              run_dir, f"identity/path disagreement: the record's input_identity prefix "
              f"{run_directory_name(recorded)!r} does not name this run directory {run_dir.name!r}"))
      continue
    if recorded != identity:
      continue  # another legitimate input identity, in its own directory
    status = record.get("status")
    if status == "completed":
      result.completed = run_dir
    elif status == "failed":
      result.preserved.append(run_dir)
    else:
      result.blocked.append((run_dir, f"the run record is not settled (status {status!r})"))
  return result


def _blocked_arm(
    *,
    case_id: str,
    manifest_path: Path,
    manifest: Manifest,
    contract: variants.ExperimentContract,
    identity: str,
    blocked: list[tuple[Path, str]],
    preserved: list[Path],
    options: ExperimentOptions,
    run_error: str | None = None,
) -> dict:
  """The arm outcome when existing evidence under this output root cannot be interpreted.

  No model call is made and nothing is deleted or replaced: the evidence stays byte-for-byte,
  the failure is visible, and the other cases and variants continue. A fresh output root is the
  intentional way to request a new draw.
  """
  evidence = [{"path": _rel_path(path, options.output_dir), "reason": reason} for path, reason in blocked]
  detail = "; ".join(f"{item['path']}: {item['reason']}" for item in evidence)
  error = (
      "existing evidence under this output root failed the identity/occupancy check, so this arm neither "
      f"ran nor reused anything and its files are preserved byte-for-byte ({detail}); resolve the evidence "
      "or use a fresh output root for a new draw")
  if run_error:
    error = f"the fresh attempt failed ({run_error}); afterwards, {error[0].lower() + error[1:]}"
  record_rel = next(
      (_rel_path(path / "run.json", options.output_dir) for path, _ in blocked if (path / "run.json").is_file()), None)
  return {
      "case": case_id,
      "manifest": str(manifest_path),
      "variant": contract.name,
      "variant_title": contract.title,
      "input_identity": identity,
      "base_commit": manifest.base_commit,
      "prompt_versions": {
          "editor": contract.editor_prompt_version,
          "reviewer": contract.reviewer_prompt_version,
      },
      "denominators": _denominators(manifest),
      "run":
          {
              "dir": None,
              "status": "blocked",
              "error": error,
              "reused": False,
              "record": record_rel,
              "proposal": None,
              "report": None,
              "preserved_failed_attempts": [_rel_path(path, options.output_dir) for path in preserved],
              "blocked_evidence": evidence,
              "usage": None,
          },
      "comparison":
          {
              "status": "blocked",
              "error": "not attempted: the arm's existing evidence failed the identity/occupancy check",
              "dir": None,
              "comparison": None,
              "report": None,
              "editor_only_status": None,
              "post_review_status": None,
              "editor_only": None,
              "post_review": None,
          },
      "editor_provenance": None,
  }


def _read_run_record(run_dir: Path) -> dict:
  return json.loads((run_dir / "run.json").read_text(encoding="utf-8"))


def _run_comparison_for(
    run_dir: Path | None,
    case_id: str,
    contract: variants.ExperimentContract,
    options: ExperimentOptions,
    cfg: CharlieBotConfig,
) -> dict:
  """Derive the paired comparison for one arm's run bundle; failures are recorded, never hidden."""
  if run_dir is None:
    return {
        "status": "absent",
        "error": "no run bundle; the arm failed before or at bundle creation",
        "dir": None,
        "comparison": None,
        "report": None,
        "editor_only_status": None,
        "post_review_status": None,
        "editor_only": None,
        "post_review": None,
    }
  comparison_dir = options.output_dir / "cases" / case_id / "comparisons" / contract.name
  try:
    run_comparison(CompareOptions(run_dir=run_dir, output_dir=comparison_dir), cfg=cfg)
  except ReplayError as e:
    return {
        "status": "failed",
        "error": str(e),
        "dir": _rel_path(comparison_dir, options.output_dir),
        "comparison": None,
        "report": None,
        "editor_only_status": None,
        "post_review_status": None,
        "editor_only": None,
        "post_review": None,
    }
  data = json.loads((comparison_dir / "comparison.json").read_text(encoding="utf-8"))
  arms = data["arms"]
  editor_only = _arm_view(run_dir, arms["editor-only"])
  post_review = _arm_view(run_dir, arms["post-review"])
  return {
      "status": "established",
      "error": None,
      "dir": _rel_path(comparison_dir, options.output_dir),
      "comparison": _rel_path(comparison_dir / "comparison.json", options.output_dir),
      "report": _rel_path(comparison_dir / "report.html", options.output_dir),
      "model_calls_made": data["model_calls_made"],
      "verification":
          {
              key:
                  (
                      data["verification"][key].get("status")
                      if isinstance(data["verification"].get(key), dict) else data["verification"][key])
              for key in sorted(data["verification"])
          },
      "editor_only_status": arms["editor-only"]["status"],
      "post_review_status": arms["post-review"]["status"],
      "editor_only": editor_only,
      "post_review": post_review,
      "shared_editor_response":
          {
              "statement": data["provenance"]["shared_editor_response"]["statement"],
              "themes": data["provenance"]["shared_editor_response"]["themes"],
          },
  }


def _arm_view(run_dir: Path, arm: dict) -> dict | None:
  """The summary's view of one comparison arm: counts, paths, dispositions, and body length."""
  if arm["status"] != "established":
    return {
        "status": "failed",
        "error": arm.get("error"),
        "counts": None,
        "changed_paths": None,
        "dispositions": None,
        "final_entry_chars": None
    }
  return {
      "status": "established",
      "error": None,
      "counts": arm["counts"],
      "changed_paths": list(arm["changed_paths"]),
      "dispositions": arm["dispositions"],
      "final_entry_chars": _final_entry_chars(run_dir, arm["reviewed_patch"]),
  }


def _final_entry_chars(run_dir: Path, reviewed_patch: str | None) -> int | None:
  """Total characters of the arm's complete final entry files, base plus patch applied."""
  if reviewed_patch is None:
    return None
  manifest = load_manifest(run_dir / "frozen" / "manifest.yaml")
  base = {path: canonical_text(text) for path, text in manifest.base_entries().items()}
  applied = apply_unified_patch(base, reviewed_patch)
  return sum(len(text) for text in applied.values() if text is not None)


def _run_section(
    run_dir: Path | None,
    record: dict | None,
    status: str,
    error: str | None,
    reused: bool,
    preserved_dirs: list[Path],
    output_root: Path,
) -> dict:
  # Every artifact ref is relative to the output root, so report.html's hrefs resolve and the
  # summary stays self-contained under one base.
  if run_dir is None or record is None:
    return {
        "dir": None,
        "status": status,
        "error": error,
        "reused": reused,
        "record": None,
        "proposal": None,
        "report": None,
        "preserved_failed_attempts": [_rel_path(path, output_root) for path in preserved_dirs],
        "blocked_evidence": [],
        "usage": None,
    }
  usage = _usage(record)
  return {
      "dir": _rel_path(run_dir, output_root),
      "status": status,
      "error": error,
      "reused": reused,
      "record": _rel_path(run_dir / "run.json", output_root),
      "proposal": _rel_path(run_dir / "proposal.json", output_root) if (run_dir / "proposal.json").is_file() else None,
      "report": _rel_path(run_dir / "report.html", output_root) if (run_dir / "report.html").is_file() else None,
      "preserved_failed_attempts": [_rel_path(path, output_root) for path in preserved_dirs],
      "blocked_evidence": [],
      "usage": usage,
  }


def _editor_provenance(run_dir: Path | None, record: dict | None, manifest: Manifest, output_root: Path) -> dict | None:
  """Per-theme call provenance of one arm's editor: every recorded attempt, the chosen one marked.

  The chosen response is the exact bytes the arm's reviewer consumed; the digest identifies
  content only — equal digests across arms mean identical content, never a shared call. The
  provenance is reported whenever the run record is readable, including when a later reviewer
  stage failed, and it accounts for every theme of the manifest, not just the first one.
  """
  if run_dir is None or record is None:
    return None
  themes: dict[str, dict] = {
      theme.name: {
          "actual_calls": 0,
          "attempts": [],
          "chosen_response": None,
      } for theme in manifest.themes
  }
  total = 0
  for call in record.get("calls", []):
    if call.get("role") != "editor":
      continue
    total += 1
    theme = themes[call["theme"]]
    theme["actual_calls"] += 1
    response_file = call.get("response_file")
    digest = sha256_hex((run_dir / response_file).read_bytes()) if response_file else None
    attempt = {
        "call": call.get("name"),
        "attempt": call.get("attempt"),
        "chosen": bool(call.get("chosen")),
        "validation": (call.get("validation") or {}).get("status"),
        "request_file": call.get("request_file"),
        "response_file": response_file,
        "response_sha256": digest,
        "output_tokens": call.get("output_tokens"),
    }
    theme["attempts"].append(attempt)
    if attempt["chosen"]:
      theme["chosen_response"] = {
          "call": attempt["call"],
          "attempt": attempt["attempt"],
          "response_file": attempt["response_file"],
          "response_sha256": attempt["response_sha256"],
      }
  for theme in themes.values():
    theme["attempts"].sort(key=lambda a: a["attempt"] or 0)
  return {
      "run_record": _rel_path(run_dir / "run.json", output_root),
      "actual_editor_calls": total,
      "themes": themes,
  }


def _rel_path(path: Path, root: Path) -> str:
  return path.resolve().relative_to(root.resolve()).as_posix()


def _build_summary(
    *,
    cases: list[tuple[str, Path, Manifest]],
    contracts: list[variants.ExperimentContract],
    model_identity: dict,
    arms: list[dict],
    now: datetime | None,
) -> dict:
  by_case: dict[str, list[dict]] = {}
  for arm in arms:
    by_case.setdefault(arm["case"], []).append(arm)
  return {
      "schema":
          EXPERIMENT_SCHEMA,
      "kind":
          "memory-curation-variant-experiment",
      "created_at":
          _timestamp(now),
      "mode":
          MODE,
      "backend":
          model_identity,
      "scope":
          {
              "control": CONTROL_NOTE,
              "editor_calls": EDITOR_CALLS_NOTE,
              "feedback_views": FEEDBACK_VIEW_NOTE,
              "quality": QUALITY_NOTE,
          },
      "adaptations":
          list(COMMON_ADAPTATIONS),
      "variants":
          [
              {
                  **contract.record_payload()["variant"],
                  # The descriptive stage labels sit beside the declared dimensions for the report.
                  "editor_stage":
                      contract.editor_stage,
                  "reviewer_stage":
                      contract.reviewer_stage,
                  "editor_system_sha256":
                      sha256_hex(contract.editor_system.encode("utf-8")),
                  "reviewer_system_sha256":
                      sha256_hex(contract.reviewer_system.encode("utf-8")),
              } for contract in contracts
          ],
      "cases":
          [
              {
                  "case": case_id,
                  "manifest": str(manifest_path),
                  "base_commit": manifest.base_commit,
                  "denominators": _denominators(manifest),
                  "arms": by_case.get(case_id, []),
                  "editor_calls": _editor_calls_section(by_case.get(case_id, [])),
              } for case_id, manifest_path, manifest in cases
          ],
  }


def _editor_calls_section(arms: list[dict]) -> dict:
  """Actual editor calls per variant arm, and byte-identical chosen content labeled as such.

  Two arms that recorded the same editor bytes still made two real calls: the grouping here is
  content equality, never shared sampling or a reused call, and each arm's usage stays its own.
  """
  per_variant: dict[str, dict] = {}
  themes: set[str] = set()
  without: list[str] = []
  for arm in arms:
    provenance = arm.get("editor_provenance") or {}
    theme_sections: dict[str, dict] = {}
    for theme, theme_provenance in (provenance.get("themes") or {}).items():
      themes.add(theme)
      chosen = theme_provenance["chosen_response"]
      theme_sections[theme] = {
          "actual_calls": theme_provenance["actual_calls"],
          "chosen_response_file": chosen["response_file"] if chosen else None,
          "chosen_response_sha256": chosen["response_sha256"] if chosen else None,
      }
    per_variant[arm["variant"]] = {
        "run_record": provenance.get("run_record"),
        "actual_editor_calls": provenance.get("actual_editor_calls", 0),
        "themes": theme_sections,
    }
    if not theme_sections or all(section["chosen_response_sha256"] is None for section in theme_sections.values()):
      without.append(arm["variant"])
  identical: dict[str, list[list[str]]] = {}
  for theme in sorted(themes):
    by_digest: dict[str, list[str]] = {}
    for variant, section in per_variant.items():
      digest = section["themes"].get(theme, {}).get("chosen_response_sha256")
      if digest:
        by_digest.setdefault(digest, []).append(variant)
    groups = sorted(sorted(group) for group in by_digest.values() if len(group) > 1)
    if groups:
      identical[theme] = groups
  return {
      "note": EDITOR_CALLS_NOTE,
      "actual_editor_calls": sum(section["actual_editor_calls"] for section in per_variant.values()),
      "per_variant": per_variant,
      "identical_chosen_content": identical,
      "variants_without_chosen_editor_response": sorted(without),
  }


# --- the readable report ---------------------------------------------------------


def render_experiment_report(summary: dict) -> str:
  """Static HTML over the summary dict; every dynamic value goes through ``html.escape``."""
  parts = [
      "<h1>Memory curation &mdash; fixed-input variant experiment</h1>",
      f'<p class="muted">{_e(summary["kind"])} · schema <code>{_e(summary["schema"])}</code> · '
      f'backend <code>{_e(str(summary["backend"]))}</code> · created {_e(summary["created_at"])}</p>',
      "<div class=\"needs\"><strong>What this experiment is and is not.</strong> "
      f'{_e(summary["scope"]["control"])} {_e(summary["scope"]["quality"])}</div>',
      "<h2>Common adaptations (every variant alike)</h2><ul>" +
      "".join(f"<li>{_e(item)}</li>" for item in summary["adaptations"]) + "</ul>",
      '<p class="muted">' + _e(summary["scope"]["feedback_views"]) + "</p>",
      "<h2>Variants</h2>",
  ]
  for variant in summary["variants"]:
    parts.append(
        "<details><summary><strong>{}</strong> &mdash; <code>{}</code></summary>".format(
            _e(variant["title"]), _e(variant["name"])))
    parts.append(
        "<table><tr><th>dimension</th><th>value</th></tr>" + "".join(
            "<tr><td>{}</td><td><code>{}</code></td></tr>".format(_e(field), _e(str(variant[field])))
            for field in ("editor_stage", "reviewer_stage", "entry_scope", "feedback_view", "rationale_visibility")) +
        "</table>")
    parts.append(
        f'<p class="mono">prompt versions: editor <code>{_e(variant["prompt_versions"]["editor"])}</code> · '
        f'reviewer <code>{_e(variant["prompt_versions"]["reviewer"])}</code></p>')
    if variant["changes_vs_baseline"]:
      parts.append("<ul>" + "".join(f"<li>{_e(change)}</li>" for change in variant["changes_vs_baseline"]) + "</ul>")
    for note in variant["notes"]:
      parts.append(f'<p class="muted">{_e(note)}</p>')
    parts.append("</details>")
  for case in summary["cases"]:
    parts.append(f"<h2>Case <code>{_e(case['case'])}</code></h2>")
    parts.append(
        f'<p class="muted">manifest <code>{_e(case["manifest"])}</code> · base_commit '
        f'<code>{_e(case["base_commit"])}</code> · themes <code>{_e(str(case["denominators"]["themes"]))}</code> · '
        f'input candidates <code>{_e(str(case["denominators"]["input_candidates"]))}</code></p>')
    parts.append(_case_arm_table(case))
    calls = case["editor_calls"]
    identical = calls["identical_chosen_content"]
    identical_text = "; ".join(
        f"{theme}: " + ", ".join("/".join(group)
                                 for group in groups)
        for theme, groups in sorted(identical.items())) or "none"
    parts.append(
        f'<p class="muted">Editor calls: {_e(str(calls["actual_editor_calls"]))} actual editor call(s) across '
        f'{_e(str(len(calls["per_variant"])))} variant arm(s); identical chosen editor content: '
        f'{_e(identical_text)} &mdash; content equality, not a shared draw or a reused call. '
        f'{_e(calls["note"])}</p>')
  parts.append(
      '<p class="muted">A failed arm keeps its recorded bundle, attempt chain, and usage under this output '
      'root and is never rerun here; a fresh output root is an intentional new draw.</p>')
  return _page("Memory curation — fixed-input variant experiment", 1200, [_EXTRA_CSS], parts)


_EXTRA_CSS = (".failed{background:#ffebe9;border:1px solid #ff818266;border-radius:6px;padding:4px 8px}")


def _case_arm_table(case: dict) -> str:
  rows = []
  for arm in case["arms"]:
    run = arm["run"]
    comparison = arm["comparison"]
    run_cell = f'<span class="failed">{_e(run["status"])}</span>' if run["status"] != "completed" else "completed"
    if run.get("reused"):
      run_cell += " (reused)"
    editor_status = _e(str(comparison["editor_only_status"])) if comparison else "-"
    post_status = _e(str(comparison["post_review_status"])) if comparison else "-"
    counts = "-"
    chars = "-"
    if comparison and comparison.get("post_review") and comparison["post_review"]["status"] == "established":
      counts = _e(str(comparison["post_review"]["counts"]))
      chars = _e(str(comparison["post_review"]["final_entry_chars"]))
    tokens = _usage_cell(run.get("usage"))
    links = []
    if run.get("proposal"):
      links.append(f'<a href="{_e(run["proposal"])}">proposal</a>')
    if run.get("report"):
      links.append(f'<a href="{_e(run["report"])}">run report</a>')
    if comparison and comparison.get("report"):
      links.append(f'<a href="{_e(comparison["report"])}">comparison</a>')
    rows.append(
        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>".format(
            f"<strong>{_e(arm['variant'])}</strong>", run_cell, editor_status, post_status, counts, chars, tokens) +
        f"<tr><td></td><td colspan=\"6\">{' · '.join(links) or '<span class=\"muted\">no artifacts</span>'}"
        f" · identity <code>{_e(str(arm['input_identity'])[:16])}</code></td></tr>")
  return (
      "<table><tr><th>variant</th><th>run</th><th>editor-only</th><th>post-review</th>"
      "<th>post-review counts</th><th>final entry chars</th><th>output tokens (editor/reviewer)</th></tr>" +
      "".join(rows) + "</table>")


def _usage_cell(usage: dict | None) -> str:
  if not usage:
    return "-"

  def tokens(section: dict) -> str:
    return str(section["output_tokens"]) if section["output_tokens"] is not None else "unknown"

  return _e(f"{tokens(usage['editor'])} / {tokens(usage['reviewer'])}")
