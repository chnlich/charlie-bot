// Message selection: the agent messages that a parent session sent to its direct children inside a time window.
//
// A child session's chat log records each message it received as an `agent_message` event whose `from_session` is
// the sender. A message counts when its sender is the child's `task_parent_id` and its timestamp falls in the
// window. The parent's own log holds the children's reports to it, which the packets read as context.

import fs from 'node:fs';
import path from 'node:path';

export function readSessionMetadata(sessionsRoot) {
  const sessions = new Map();
  for (const sid of fs.readdirSync(sessionsRoot).sort()) {
    const metaPath = path.join(sessionsRoot, sid, 'metadata.json');
    if (!fs.existsSync(metaPath)) continue;
    sessions.set(sid, JSON.parse(fs.readFileSync(metaPath, 'utf8')));
  }
  return sessions;
}

// The `agent_message` events of one chat log. A line that does not parse is logged with its count and skipped.
export function readAgentMessages(logPath) {
  const events = [];
  let malformed = 0;
  const lines = fs.readFileSync(logPath, 'utf8').split('\n');
  for (const line of lines) {
    if (!line.includes('"agent_message"')) continue;
    let event;
    try {
      event = JSON.parse(line);
    } catch (error) {
      if (!(error instanceof SyntaxError)) throw error;
      malformed += 1;
      continue;
    }
    if (event.type === 'agent_message') events.push(event);
  }
  if (malformed > 0) console.error(`skipped ${malformed} malformed agent_message line(s) in ${logPath}`);
  return events;
}

export function inWindow(timestamp, fromMs, toMs) {
  const ms = Date.parse(timestamp);
  if (Number.isNaN(ms)) throw new Error(`event timestamp ${JSON.stringify(timestamp)} does not parse`);
  return ms >= fromMs && ms < toMs;
}

// Returns one record per window message, sorted by send time. `content` is the message text as the child recorded it.
export function windowMessages({ sessionsRoot, sessions, fromMs, toMs }) {
  const messages = [];
  for (const [child, meta] of sessions) {
    const parent = meta.task_parent_id;
    if (!parent) continue;
    const logPath = path.join(sessionsRoot, child, 'data', 'chat_events.jsonl');
    if (!fs.existsSync(logPath)) continue;
    const parentMeta = sessions.get(parent);
    if (!parentMeta) throw new Error(`child ${child} names parent ${parent}, which has no metadata.json`);
    for (const event of readAgentMessages(logPath)) {
      if (event.from_session !== parent) continue;
      if (!inWindow(event.timestamp, fromMs, toMs)) continue;
      messages.push({
        event_id: event.id,
        sender: parent,
        sender_name: parentMeta.name ?? null,
        target: child,
        target_meta: meta,
        sent_at: event.timestamp,
        content: event.content ?? '',
      });
    }
  }
  messages.sort((a, b) => Date.parse(a.sent_at) - Date.parse(b.sent_at));
  return messages;
}

// A child's reports to its parent in the window: the parent's agent messages whose `from_session` is the child.
// `parentEvents` is the parent's `agent_message` list, read once per parent.
export function reportsFrom({ parentEvents, child, fromMs, toMs }) {
  return parentEvents
    .filter((event) => event.from_session === child && inWindow(event.timestamp, fromMs, toMs))
    .map((event) => ({ kind: 'report', event_id: event.id, sent_at: event.timestamp, content: event.content ?? '' }));
}

export function parentEventsOf({ sessionsRoot, parent }) {
  return readAgentMessages(path.join(sessionsRoot, parent, 'data', 'chat_events.jsonl'));
}
