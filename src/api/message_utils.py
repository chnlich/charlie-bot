"""Shared helpers for converting raw chat events into displayable messages."""


def events_to_messages(events: list[dict]) -> list[dict]:
  """Convert raw chat_events.jsonl entries into displayable messages."""
  messages = []
  assistant_buf = ""
  last_event_idx = 0

  for idx, ev in enumerate(events):
    t = ev.get("type")
    if t == "user":
      # Skip CC-internal user events (tool results) — they have a "message" field
      # but no top-level "content". Only real user messages have "content".
      if "message" in ev and "content" not in ev:
        continue
      # Flush any pending assistant buffer
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf, "event_index": last_event_idx})
        assistant_buf = ""
      messages.append(
          {
              "role": "user",
              "content": ev.get("content", ""),
              "is_voice": ev.get("is_voice", False),
              "event_index": idx,
          })
    elif t == "assistant":
      last_event_idx = idx
      msg = ev.get("message") or {}
      blocks = msg.get("content") or []
      for b in blocks:
        if isinstance(b, dict) and b.get('type') == 'tool_use' and b.get('name') == 'ExitPlanMode':
          plan_text = (b.get('input') or {}).get('plan', '')
          if plan_text:
            if assistant_buf:
              messages.append({'role': 'assistant', 'content': assistant_buf, 'event_index': last_event_idx})
              assistant_buf = ''
            messages.append({'role': 'plan', 'content': plan_text, 'event_index': idx})
          elif assistant_buf:
            messages.append({'role': 'plan', 'content': assistant_buf, 'event_index': idx})
            assistant_buf = ''
      text = "".join(b.get("text", "") for b in blocks if isinstance(b, dict) and b.get("type") == "text")
      if text and assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf, "event_index": last_event_idx})
        assistant_buf = ""
      assistant_buf += text
    elif t == "master_done":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf, "event_index": last_event_idx})
        assistant_buf = ""
      if not ev.get("still_thinking"):
        messages.append({"role": "separator", "thinking_seconds": ev.get("thinking_seconds"), "event_index": idx})
    elif t == "assistant_error":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf, "event_index": last_event_idx})
        assistant_buf = ""
      messages.append({"role": "system", "content": f"Error: {ev.get('content', '')}", "event_index": idx})
    elif t == "error":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf, "event_index": last_event_idx})
        assistant_buf = ""
      error_content = ev.get("content") or ev.get("message") or "Unknown error"
      messages.append({"role": "system", "content": f"Error: {error_content}", "event_index": idx})
    elif t == "task_delegated":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf, "event_index": last_event_idx})
        assistant_buf = ""
      desc = ev.get("description", "")
      messages.append({"role": "system", "content": f"Task delegated: {desc}", "event_index": idx})
    elif t == "worker_summary":
      if assistant_buf:
        messages.append({"role": "assistant", "content": assistant_buf, "event_index": last_event_idx})
        assistant_buf = ""
      messages.append({
          "role": "worker_summary",
          "content": ev.get("content", ""),
          "full_content": ev.get("full_content", ""),
          "event_index": idx,
      })
    elif t == 'handler_result':
      if assistant_buf:
        messages.append({'role': 'assistant', 'content': assistant_buf, 'event_index': last_event_idx})
        assistant_buf = ''
      icon = '\u2713' if ev.get('status') == 'ok' else '\u2717'
      messages.append({
          'role': 'system',
          'content': f"{icon} {ev.get('task', '')}: {ev.get('message', '')}",
          'event_index': idx,
      })
    elif t == 'context_compacted':
      if assistant_buf:
        messages.append({'role': 'assistant', 'content': assistant_buf, 'event_index': last_event_idx})
        assistant_buf = ''
      trigger = ev.get('trigger', 'auto')
      pre_tokens = ev.get('pre_tokens')
      msg = 'Context compacted'
      if trigger:
        msg += f' ({trigger})'
      if pre_tokens:
        msg += f' \u2014 was {round(pre_tokens / 1000)}k tokens'
      messages.append({'role': 'system', 'content': msg, 'event_index': idx})

  # Flush trailing assistant content (if stream was interrupted)
  if assistant_buf:
    messages.append({"role": "assistant", "content": assistant_buf, "event_index": last_event_idx})

  return messages
