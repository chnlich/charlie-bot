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
- repeating the experiment reuses completed bundles without model calls and preserves the
  recorded failures;
- variants draw their own editor responses; the summary discloses which arms shared a
  byte-identical editor response and which did not, and never claims a single stochastic draw
  isolates a causal gain;
- semantic quality stays unjudged: fewer lines, fewer proposals, or more deletions are data,
  never an automatic pass, and the exit code reports execution and format success only.
"""

import json
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

import structlog

from src.core.config import CharlieBotConfig, get_config
from src.core.memory_replay import variants
from src.core.memory_replay.compare import CompareOptions, _usage, run_comparison
from src.core.memory_replay.errors import ReplayError
from src.core.memory_replay.identity import sha256_hex
from src.core.memory_replay.manifest import REF_RE, Manifest, load_manifest
from src.core.memory_replay.report import _e, _page
from src.core.memory_replay.runner import (
    ReplayOptions,
    _require_disjoint_output_root,
    compute_input_identity,
    resolve_backend_identity,
    run_replay,
)
from src.core.memory_replay.validate import apply_unified_patch, canonical_text

log = structlog.get_logger()

EXPERIMENT_SCHEMA = "memory-curation-variant-experiment/1"
MODE = "editor-review"

QUALITY_NOTE = (
    "No semantic quality judgment is made here: fewer lines, fewer proposals, or more deletions never "
    "establish better curation. An independent assessment supplies quality; this experiment supplies "
    "execution evidence.")
STOCHASTIC_NOTE = (
    "Each variant ran its own editor draw unless the recorded editor responses are byte-identical. "
    "Separate draws confound editor stochasticity with the declared intervention, so a single-draw "
    "difference between variants does not isolate a causal gain; arms that genuinely shared a response "
    "record its source and usage honestly.")
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

  summary = _build_summary(
      options=options, cases=cases, contracts=contracts, model_identity=model_identity, arms=arms, now=now)
  options.output_dir.mkdir(parents=True, exist_ok=True)
  summary_path = options.output_dir / "experiment.json"
  report_path = options.output_dir / "report.html"
  summary_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
  report_path.write_text(render_experiment_report(summary), encoding="utf-8")
  failed_arms = [
      f"{arm['case']}/{arm['variant']}" for arm in arms
      if arm["run"]["status"] != "completed" or arm["comparison"]["status"] == "failed"
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
  """One case x variant: reuse, run, or preserve the recorded failure — then the paired comparison."""
  identity = compute_input_identity(manifest, mode=MODE, model_identity=model_identity, contract=contract)
  variant_runs_root = options.output_dir / "cases" / case_id / "runs" / contract.name
  runs_dir = variant_runs_root / "runs"
  completed_dir, preserved_dirs = _scan_existing_runs(runs_dir, identity)

  run_error: str | None = None
  run_dir: Path | None = completed_dir or (preserved_dirs[-1] if preserved_dirs else None)
  reused = completed_dir is not None
  if completed_dir is None and not preserved_dirs:
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
      completed_dir, preserved_dirs = _scan_existing_runs(runs_dir, identity)
      run_dir = completed_dir or (preserved_dirs[-1] if preserved_dirs else None)

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
          _run_section(run_dir, record, run_status, run_status_error, reused, preserved_dirs, options.output_dir),
      "comparison":
          comparison,
  }
  if record and record.get("status") == "completed" and run_dir is not None:
    arm["editor_response_sha256"] = _chosen_response_sha256(run_dir, record, "editor")
    arm["reviewer_response_sha256"] = _chosen_response_sha256(run_dir, record, "reviewer")
  else:
    arm["editor_response_sha256"] = None
    arm["reviewer_response_sha256"] = None
  return arm


def _scan_existing_runs(runs_dir: Path, identity: str) -> tuple[Path | None, list[Path]]:
  """Completed and preserved (failed or killed) run bundles with this exact identity.

  A completed bundle is reused without model calls. Anything else that already settled under
  this output root stays exactly as recorded — a failed or killed attempt is never silently
  deleted and rerun here; a fresh output root is the intentional new draw.
  """
  completed: Path | None = None
  preserved: list[Path] = []
  if not runs_dir.is_dir():
    return None, preserved
  for record_path in sorted(runs_dir.glob("*/run.json")):
    try:
      record = json.loads(record_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
      raise ReplayError(f"experiment run record {record_path} is unreadable: {e}") from e
    if record.get("input_identity") != identity:
      continue
    if record.get("status") == "completed":
      completed = record_path.parent
    else:
      preserved.append(record_path.parent)
  return completed, preserved


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
      "usage": usage,
  }


def _chosen_response_sha256(run_dir: Path, record: dict, role: str) -> str | None:
  for call in record.get("calls", []):
    if call.get("role") == role and call.get("chosen") and call.get("response_file"):
      return sha256_hex((run_dir / call["response_file"]).read_bytes())
  return None


def _denominators(manifest: Manifest) -> dict:
  per_theme = {theme.name: len(theme.candidate_refs) for theme in manifest.themes}
  return {
      "themes": len(manifest.themes),
      "theme_names": [theme.name for theme in manifest.themes],
      "input_candidates": sum(per_theme.values()),
      "candidates_per_theme": per_theme,
  }


def _rel_path(path: Path, root: Path) -> str:
  return path.resolve().relative_to(root.resolve()).as_posix()


def _build_summary(
    *,
    options: ExperimentOptions,
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
              "stochastic_editor_draws": STOCHASTIC_NOTE,
              "feedback_views": FEEDBACK_VIEW_NOTE,
              "quality": QUALITY_NOTE,
          },
      "adaptations":
          list(COMMON_ADAPTATIONS),
      "variants":
          [
              {
                  **contract.record_payload()["variant"],
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
                  "editor_draws": _editor_draws(by_case.get(case_id, [])),
              } for case_id, manifest_path, manifest in cases
          ],
  }


def _editor_draws(arms: list[dict]) -> dict:
  """Which variants of this case genuinely shared one recorded editor response, and which did not."""
  by_hash: dict[str, list[str]] = {}
  missing: list[str] = []
  for arm in arms:
    digest = arm.get("editor_response_sha256")
    if digest is None:
      missing.append(arm["variant"])
    else:
      by_hash.setdefault(digest, []).append(arm["variant"])
  shared = sorted([sorted(names) for names in by_hash.values() if len(names) > 1])
  return {
      "note": STOCHASTIC_NOTE,
      "shared_editor_responses": shared,
      "distinct_editor_responses": len(by_hash),
      "variants_without_editor_response": sorted(missing),
  }


def _timestamp(now: datetime | None) -> str:
  return (now or datetime.now(UTC)).strftime("%Y-%m-%dT%H:%M:%SZ")


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
            "<tr><td>{}</td><td><code>{}</code></td></tr>".format(_e(field), _e(str(variant[field]))) for field in
            ("editor_stage", "reviewer_stage", "feedback_view", "rationale_visibility", "reviewer_capability")) +
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
    draws = case["editor_draws"]
    parts.append(
        f'<p class="muted">Editor draws: {_e(str(draws["distinct_editor_responses"]))} distinct recorded '
        f'editor response(s); shared byte-identical responses: '
        f'{_e(", ".join("/".join(group) for group in draws["shared_editor_responses"])) or "none"}. '
        f'{_e(draws["note"])}</p>')
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
