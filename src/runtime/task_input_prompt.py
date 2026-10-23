"""Compose the text and uploads for one task Run's durable input events."""

from __future__ import annotations

from src.infra import event_types as ET
from src.infra import models
from src.runtime import message_events
from src.runtime.hooks import turn_contributions


def child_report_text(report: dict, provenance: str | None = None) -> str:
  """Format a child report as the parent's typed task input."""
  header = f"[Report from task {report.get('child_session_id')}"
  if provenance:
    header += f" ({provenance})"
  return f"{header} | outcome {report.get('outcome')}] {str(report.get('summary') or '')}"


def compose_input_prompt(
    meta: models.SessionMetadata,
    events: list[dict],
    report_provenance: dict[str, str] | None = None,
) -> tuple[str, list[dict]]:
  """Return the text and native uploads for one exact input batch."""
  parts: list[str] = []
  uploads: list[dict] = []
  for event in events:
    event_type = event.get("type")
    content = str(event.get("content") or "")
    if event_type == ET.USER:
      preamble = turn_contributions.resolve_input_preamble(meta, event)
      text = content if preamble is None else f"{preamble}\n{content}"
    elif event_type == ET.AGENT_MESSAGE:
      text = (
          f"[Message from session {event.get('from_session_name') or event.get('from_session') or 'unknown'}] "
          f"{content}")
    elif event_type == ET.CHILD_REPORT:
      provenance = (report_provenance or {}).get(str(event.get("child_session_id")))
      text = child_report_text(event, provenance)
    elif event_type == ET.SCHEDULED_TRIGGER:
      text = f"[Scheduled trigger] {content}"
    else:
      text = content
    event_uploads = event.get("uploaded_files") or []
    parts.append(message_events.build_agent_input_content(text, event_uploads))
    uploads.extend(event_uploads)
  return "\n\n".join(part for part in parts if part), uploads
