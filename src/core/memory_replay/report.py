"""The proposal review page: final diff and dispositions first, evidence folded.

Everything dynamic goes through ``html.escape`` — entry bodies, comments, and
patch lines are arbitrary text and must never become markup. The page is
static HTML by design: no scripts, no external assets.

The evidence sections describe what the run's stages actually received, and the
facts come from the run's owning contract via the runner — the renderer keeps
no registry of its own. The editor section states whether the reviewer request
carried the editor's handoff (its disposition rows and proof lines), withheld
it, or never ran (editor-only mode); the feedback section renders the feedback
view the stages actually read — the whole raw comment pool verbatim under the
raw-history view, or the relevance selection with approved before/after texts
under the selected structured view. Bundled-but-unexposed material — approved
revisions under the raw-history view, pool comments the selection did not pick
— is labeled as never provided, never presented as model-visible evidence.
"""

import html
from dataclasses import dataclass, field

from src.core.memory_replay.manifest import FeedbackExample, Source
from src.core.memory_replay.retrieval import FeedbackSelection
from src.core.memory_replay.variants import (
    RATIONALE_HIDDEN,
    RATIONALE_VISIBLE,
    RAW_HISTORY_VIEW,
    SELECTED_STRUCTURED_VIEW,
)

# <style> rules shared by the memory-replay HTML pages; _page wraps them with
# each page's body-width pin and page-only rules.
REPORT_CSS = [
    "h1{font-size:20px}h2{font-size:15px;border-bottom:1px solid #d9dfe6;padding-bottom:4px;margin-top:26px}",
    "table{border-collapse:collapse;width:100%;font-size:12.5px}",
    "th,td{border:1px solid #d9dfe6;padding:6px 9px;text-align:left;vertical-align:top}",
    "th{background:#eef1f5}code,.mono{font-family:ui-monospace,Menlo,Consolas,monospace;font-size:12px}",
    "pre{background:#f6f8fa;border:1px solid #d9dfe6;border-radius:6px;padding:10px;overflow:auto;"
    "font-family:ui-monospace,Menlo,Consolas,monospace;font-size:12px;white-space:pre-wrap}",
    ".needs{background:#fff4e5;border:1px solid #e6d09b;border-radius:6px;padding:10px 14px;margin:8px 0}",
    ".muted{color:#5b6774;font-size:12px}",
]


def _page(title: str, body_width_px: int, extra_css: list[str], body_parts: list[str]) -> str:
  """One static memory-replay page, newline-joined with a trailing newline.

  *title* is a static literal (never user text). *body_width_px* and *extra_css*
  are the per-page pins around REPORT_CSS; the caller pre-escapes every dynamic
  body value with :func:`_e`, so body_parts are trusted markup fragments.
  """
  parts = [
      "<!doctype html>",
      '<html lang="en">',
      "<head>",
      '<meta charset="utf-8">',
      f"<title>{title}</title>",
      "<style>",
      f"body{{font:14px/1.5 -apple-system,sans-serif;margin:24px auto;max-width:{body_width_px}px;color:#1b2430}}",
      *REPORT_CSS,
      *extra_css,
      "</style>",
      "</head>",
      "<body>",
      *body_parts,
      "</body></html>",
  ]
  return "\n".join(parts) + "\n"


@dataclass
class ReportData:
  """Everything the report renders, already computed by the runner.

  ``feedback_view`` and ``rationale_visibility`` are the run's owning contract's
  declared dimensions; ``feedback_examples`` is the frozen comment pool both
  views render from. The renderer phrases these facts but never decides them.
  """

  mode: str
  created_at: str
  base_commit: str
  input_identity: str
  model_identity: dict
  prompt_versions: dict
  patch: str
  candidate_results: list[dict]
  changed_mapping: list[dict]
  sources: list[Source]
  selections: dict[str, list[FeedbackSelection]]
  editor_dispositions: list[dict]
  calls: list[dict]
  feedback_view: str
  rationale_visibility: str
  feedback_examples: list[FeedbackExample]
  unused_sources: list[str] = field(default_factory=list)


def render_report(data: ReportData) -> str:
  needs_decision = [row for row in data.candidate_results if row["outcome"] == "needs_decision"]
  parts = [
      "<h1>Memory replay proposal</h1>",
      f'<p class="muted">mode <code>{_e(data.mode)}</code> · base_commit <code>{_e(data.base_commit)}</code> · '
      f'model <code>{_e(str(data.model_identity))}</code> · input identity <code>{_e(data.input_identity[:16])}</code> · '
      f'created {_e(data.created_at)}</p>',
      "<h2>Final diff</h2>",
      f'<pre id="final-diff">{_e(data.patch) if data.patch else "(no changes)"}</pre>',
  ]
  if needs_decision:
    parts.append("<h2>Needs decision</h2>")
    for row in needs_decision:
      parts.append(
          f'<div class="needs"><strong>{_e(row["source_ref"])}</strong> — {_e(row["reason"])} '
          f'<span class="muted">(paths: {_e(", ".join(row["paths"])) or "none"})</span></div>')
  parts.append("<h2>Final dispositions</h2>")
  parts.append(
      "<table><tr><th>source_ref</th><th>outcome</th><th>paths</th><th>reason</th></tr>" + "".join(
          _row(_e(row["source_ref"]), _e(row["outcome"]), _e(", ".join(row["paths"])), _e(row["reason"]))
          for row in data.candidate_results) + "</table>")
  parts.append("<h2>Changed paths &rarr; evidence</h2>")
  if data.changed_mapping:
    parts.append(
        "<table><tr><th>path</th><th>claiming dispositions</th></tr>" + "".join(
            _row(_e(item["path"]), _e("; ".join(f"{d['source_ref']} ({d['outcome']})"
                                                for d in item["dispositions"])))
            for item in data.changed_mapping) + "</table>")
  else:
    parts.append('<p class="muted">No paths changed.</p>')
  parts.append("<h2>Evidence</h2>")
  parts.extend(_sources_section(data.sources))
  parts.extend(_feedback_section(data))
  parts.extend(_editor_section(data))
  parts.extend(_run_section(data))
  return _page("Memory replay proposal", 1100, [], parts)


def _sources_section(sources: list[Source]) -> list[str]:
  lines = [f"<details><summary>Sources ({len(sources)})</summary>"]
  for source in sources:
    path_note = f" · path <code>{_e(source.path)}</code>" if source.path else ""
    lines.append(
        f"<p><code>{_e(source.ref)}</code> · {_e(source.kind)}{path_note} · "
        f'sha256 <code>{_e(source.sha256[:12])}&hellip;</code></p>'
        f"<details><summary>snapshot</summary><pre>{_e(source.text)}</pre></details>")
  lines.append("</details>")
  return lines


def _feedback_section(data: ReportData) -> list[str]:
  """The feedback view the stages actually read, per the run's declared feedback view."""
  if data.feedback_view == RAW_HISTORY_VIEW:
    return _raw_history_feedback_section(data)
  if data.feedback_view == SELECTED_STRUCTURED_VIEW:
    return _selected_feedback_section(data)
  raise ValueError(f"unknown feedback view: {data.feedback_view!r}")


def _selected_feedback_section(data: ReportData) -> list[str]:
  selections = data.selections
  total = sum(len(v) for v in selections.values())
  lines = [
      f"<details><summary>Selected feedback ({total}) &mdash; the selected structured view, "
      f"{_stage_reach(data.mode)}</summary>",
      '<p class="muted">What was provided is exactly this relevance selection: each selected comment\'s original '
      "text with its provenance id and, when one exists, the approved before/after revision. Pool comments not "
      "listed below stayed in the frozen pool and were never provided.</p>",
  ]
  for theme in sorted(selections):
    lines.append(f"<p><strong>{_e(theme)}</strong></p>")
    if not selections[theme]:
      lines.append('<p class="muted">No prior comment matched this theme.</p>')
    for selection in selections[theme]:
      example = selection.example
      change = example.approved_change
      lines.append(
          f"<p><code>{_e(example.comment_event)}</code> · score {selection.score} · "
          f"principles {_e(', '.join(selection.matched_principles)) or '-'} · "
          f"terms {_e(', '.join(selection.matched_terms)) or '-'}</p>")
      lines.append(f"<pre>comment:\n{_e(example.comment_text)}</pre>")
      if change is not None:
        lines.append(
            f"<pre>approved change (ref {_e(change.approved_change_ref)}):\n--- before ---\n{_e(change.before)}\n"
            f"--- after ---\n{_e(change.after)}</pre>")
  provided = {s.example.comment_event for selected in selections.values() for s in selected}
  unselected = sorted(
      example.comment_event for example in data.feedback_examples if example.comment_event not in provided)
  if unselected:
    lines.append(
        '<p class="muted">In the frozen pool but not selected, so never provided to any stage: '
        f"{_e(', '.join(unselected))}</p>")
  lines.append("</details>")
  return lines


def _raw_history_feedback_section(data: ReportData) -> list[str]:
  pool = data.feedback_examples
  approved_count = sum(1 for example in pool if example.approved_change is not None)
  lines = [
      f"<details><summary>Feedback history (raw) ({len(pool)} pool comments) &mdash; the raw view, "
      f"{_stage_reach(data.mode)}</summary>",
      '<p class="muted">The raw-history view is the frozen-input replacement for the production selector\'s '
      "user-message digest: every pool comment verbatim with its provenance id, nothing else. It carries no "
      "approved before/after revisions" + (
          f"; the frozen pool bundles {approved_count} approved revision(s), and the stages saw none of them."
          if approved_count else ".") + "</p>",
  ]
  for example in pool:
    lines.append(f"<p><code>{_e(example.comment_event)}</code></p>")
    lines.append(f"<pre>comment:\n{_e(example.comment_text)}</pre>")
  lines.append(
      "<details><summary>Relevance-selection audit (computed by the replay's retrieval and recorded in the run "
      "record; not part of the raw view the stages saw)</summary>")
  for theme in sorted(data.selections):
    lines.append(f"<p><strong>{_e(theme)}</strong></p>")
    if not data.selections[theme]:
      lines.append('<p class="muted">No prior comment matched this theme.</p>')
    for selection in data.selections[theme]:
      lines.append(
          f"<p><code>{_e(selection.example.comment_event)}</code> · score {selection.score} · "
          f"principles {_e(', '.join(selection.matched_principles)) or '-'} · "
          f"terms {_e(', '.join(selection.matched_terms)) or '-'}</p>")
  lines.append("</details>")
  lines.append("</details>")
  return lines


def _editor_section(data: ReportData) -> list[str]:
  """The editor's own rows and proofs, captioned by what the run's second stage actually received."""
  if data.mode == "editor-only":
    summary_note = "editor-only run: no second review ran, so no reviewer received any of this"
    proof_caption = "proofs (model output; no reviewer ran in this editor-only run):"
  elif data.rationale_visibility == RATIONALE_VISIBLE:
    summary_note = "the reviewer request carried this handoff: the disposition rows and their proof lines below"
    proof_caption = "proofs (model output; the reviewer request carried them under the run's visible-rationale setting):"
  elif data.rationale_visibility == RATIONALE_HIDDEN:
    summary_note = (
        "admission rationale withheld from the reviewer: the reviewer request carried the proposed "
        "entries only, never these rows or proof lines")
    proof_caption = "proofs (model output, withheld from the reviewer by the run's hidden-rationale setting):"
  else:
    raise ValueError(f"unknown rationale visibility: {data.rationale_visibility!r}")
  lines = [
      "<details><summary>Editor dispositions &mdash; audit record "
      f"({len(data.editor_dispositions)} rows; {summary_note})</summary>"
  ]
  for row in data.editor_dispositions:
    lines.append(
        f"<p><code>{_e(row['role'])}</code> · {_e(row['kind'])} · <code>{_e(row['name'])}</code> · "
        f"{_e(row['detail'])}</p>")
    proofs = row.get("proofs")
    if proofs:
      lines.append(
          f"<pre>{proof_caption}\n"
          f"action: {_e(proofs['action'])}\nhome: {_e(proofs['home'])}\n"
          f"brevity: {_e(proofs['brevity'])}</pre>")
  lines.append("</details>")
  return lines


def _stage_reach(mode: str) -> str:
  """How far the feedback view traveled, in the run's own mode."""
  if mode == "editor-review":
    return "provided to both stages"
  if mode == "editor-only":
    return "provided to the editor only (this run ran no reviewer)"
  raise ValueError(f"unknown replay mode: {mode!r}")


def _run_section(data: ReportData) -> list[str]:
  """Every recorded model attempt: its validation outcome, whether it was chosen, and its usage."""
  lines = [f"<details><summary>Run record ({len(data.calls)} model attempts)</summary>"]
  lines.append(
      "<table><tr><th>call</th><th>attempt</th><th>outcome</th><th>chosen</th>"
      "<th>latency ms</th><th>output tokens</th><th>cost</th></tr>")
  for call in data.calls:
    validation = call.get("validation") or {}
    outcome = str(validation.get("status"))
    lines.append(
        _row(
            _e(str(call["name"])), _e(str(call.get("attempt"))), _e(outcome), _e("yes" if call.get("chosen") else "no"),
            _e("unknown" if call.get("latency_ms") is None else str(call["latency_ms"])),
            _e("unknown" if call.get("output_tokens") is None else str(call["output_tokens"])),
            _e("unknown (endpoint reports no pricing)")))
  lines.append("</table>")
  lines.append(f'<p class="mono">prompt versions: {_e(str(data.prompt_versions))}</p>')
  if any(len([c
              for c in data.calls
              if c["role"] == role and c["theme"] == theme]) > 1
         for role in ("editor", "reviewer")
         for theme in {c["theme"] for c in data.calls}):
    lines.append(
        '<p class="muted">A recovered response is stage execution/recovery after a mechanical '
        'validation failure &mdash; not independent-review quality gain.</p>')
  if data.unused_sources:
    lines.append(
        '<p class="muted">Sources not assigned to any theme (inert): '
        f"{_e(', '.join(data.unused_sources))}</p>")
  lines.append("</details>")
  return lines


def _e(value: str) -> str:
  return html.escape(value, quote=True)


def _row(*cells: object) -> str:
  """One plain report-table row: the cells inside one <tr>, each wrapped in a <td>.

  Cells arrive escaped or pre-built: the ``_e`` call stays at the site that
  knows whether the value is trusted, so a spanning cell keeps its own markup.
  """
  return "<tr>" + "".join(f"<td>{cell}</td>" for cell in cells) + "</tr>"
