"""The proposal review page: final diff and dispositions first, evidence folded.

Everything dynamic goes through ``html.escape`` — entry bodies, comments, and
patch lines are arbitrary text and must never become markup. The page is
static HTML by design: no scripts, no external assets.
"""

import html
from dataclasses import dataclass, field

from src.core.memory_replay.manifest import Source
from src.core.memory_replay.retrieval import FeedbackSelection


@dataclass
class ReportData:
  """Everything the report renders, already computed by the runner."""

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
  unused_sources: list[str] = field(default_factory=list)


def render_report(data: ReportData) -> str:
  needs_decision = [row for row in data.candidate_results if row["outcome"] == "needs_decision"]
  parts = [
      "<!doctype html>",
      '<html lang="en">',
      "<head>",
      '<meta charset="utf-8">',
      "<title>Memory replay proposal</title>",
      "<style>",
      "body{font:14px/1.5 -apple-system,sans-serif;margin:24px auto;max-width:1100px;color:#1b2430}",
      "h1{font-size:20px}h2{font-size:15px;border-bottom:1px solid #d9dfe6;padding-bottom:4px;margin-top:26px}",
      "table{border-collapse:collapse;width:100%;font-size:12.5px}",
      "th,td{border:1px solid #d9dfe6;padding:6px 9px;text-align:left;vertical-align:top}",
      "th{background:#eef1f5}code,.mono{font-family:ui-monospace,Menlo,Consolas,monospace;font-size:12px}",
      "pre{background:#f6f8fa;border:1px solid #d9dfe6;border-radius:6px;padding:10px;overflow:auto;"
      "font-family:ui-monospace,Menlo,Consolas,monospace;font-size:12px;white-space:pre-wrap}",
      ".needs{background:#fff4e5;border:1px solid #e6d09b;border-radius:6px;padding:10px 14px;margin:8px 0}",
      ".muted{color:#5b6774;font-size:12px}",
      "</style>",
      "</head>",
      "<body>",
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
          "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>".format(
              _e(row["source_ref"]), _e(row["outcome"]), _e(", ".join(row["paths"])), _e(row["reason"]))
          for row in data.candidate_results) + "</table>")
  parts.append("<h2>Changed paths &rarr; evidence</h2>")
  if data.changed_mapping:
    parts.append(
        "<table><tr><th>path</th><th>claiming dispositions</th></tr>" + "".join(
            "<tr><td>{}</td><td>{}</td></tr>".format(
                _e(item["path"]), _e("; ".join(f"{d['source_ref']} ({d['outcome']})"
                                               for d in item["dispositions"])))
            for item in data.changed_mapping) + "</table>")
  else:
    parts.append('<p class="muted">No paths changed.</p>')
  parts.append("<h2>Evidence</h2>")
  parts.extend(_sources_section(data.sources))
  parts.extend(_feedback_section(data.selections))
  parts.extend(_editor_section(data.editor_dispositions))
  parts.extend(_run_section(data))
  parts.append("</body></html>")
  return "\n".join(parts) + "\n"


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


def _feedback_section(selections: dict[str, list[FeedbackSelection]]) -> list[str]:
  total = sum(len(v) for v in selections.values())
  lines = [f"<details><summary>Selected feedback ({total})</summary>"]
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
  lines.append("</details>")
  return lines


def _editor_section(editor_dispositions: list[dict]) -> list[str]:
  lines = [
      "<details><summary>Editor dispositions &mdash; audit record "
      f"({len(editor_dispositions)} rows; admission rationale, withheld from the reviewer)</summary>"
  ]
  for row in editor_dispositions:
    lines.append(
        f"<p><code>{_e(row['role'])}</code> · {_e(row['kind'])} · <code>{_e(row['name'])}</code> · "
        f"{_e(row['detail'])}</p>")
  lines.append("</details>")
  return lines


def _run_section(data: ReportData) -> list[str]:
  lines = [f"<details><summary>Run record ({len(data.calls)} model calls)</summary>"]
  lines.append("<table><tr><th>call</th><th>latency ms</th><th>output tokens</th><th>cost</th></tr>")
  for call in data.calls:
    lines.append(
        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>".format(
            _e(call["name"]), _e(str(call["latency_ms"])),
            _e("unknown" if call["output_tokens"] is None else str(call["output_tokens"])),
            _e("unknown (endpoint reports no pricing)")))
  lines.append("</table>")
  lines.append(f'<p class="mono">prompt versions: {_e(str(data.prompt_versions))}</p>')
  if data.unused_sources:
    lines.append(
        '<p class="muted">Sources not assigned to any theme (inert): '
        f"{_e(', '.join(data.unused_sources))}</p>")
  lines.append("</details>")
  return lines


def _e(value: str) -> str:
  return html.escape(value, quote=True)
