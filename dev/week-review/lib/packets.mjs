// One packet per child session and chunk: the child's task record, then its messages in time order, each parent message
// followed by the local files it points to. A packet holds up to PACKET_LIMIT characters, so a long exchange splits.
//
// Within one chunk a file that repeats an earlier message's file shows only a pointer to that message, and a file in
// the child's own session shows its name only.

import { FILE_CAP, isMerged } from './files.mjs';

export const PACKET_LIMIT = 120000;
const REPORT_CAP = 3000;
const DEDUPE_PREFIX = 2000;

function codePoints(text) {
  return Array.from(text);
}

function length(text) {
  return codePoints(text).length;
}

function shown(value) {
  return String(value ?? 'None');
}

function headerLines({ child, meta, parent, parentName }) {
  const task = meta.task ?? {};
  return [
    `# Child session ${child}`,
    `Name: ${shown(meta.name)}`,
    `Profile: ${shown(meta.profile)}`,
    `Parent session: ${parent} (${shown(parentName)})`,
    '',
    '## Child task record',
    `Goal: ${shown(task.goal)}`,
    'Acceptance:',
    ...(task.acceptance ?? []).map((item) => `- ${item}`),
    'Context refs:',
    ...(task.context_refs ?? []).map((item) => `- ${item}`),
    '',
    '## Messages in time order',
  ];
}

// Renders one parent message; `seen` maps each merged file's key to the message that first showed it in this chunk.
function renderMessage(item, seen) {
  const parts = [`\n### PARENT_MESSAGE id=${item.event_id} ts=${item.sent_at}`, item.content];
  for (const file of item.files) {
    if (file.state === 'own') {
      parts.push(`\n#### FILE ${file.path} (the child's own file, written in the child's session; shown by name only)`);
      continue;
    }
    if (!isMerged(file)) continue;
    const key = JSON.stringify([file.path, codePoints(file.text).slice(0, DEDUPE_PREFIX).join('')]);
    if (seen.has(key)) {
      parts.push(`\n#### FILE ${file.path} (part of this message; same content as in message ${seen.get(key)} above)`);
      continue;
    }
    seen.set(key, item.event_id);
    const note = file.state === 'truncated' ? ` (first ${FILE_CAP} of ${file.chars} chars)` : '';
    parts.push(`\n#### FILE ${file.path}${note} (part of this message)\n${file.text}`);
  }
  return parts.join('\n');
}

function renderReport(item) {
  const content = codePoints(item.content).slice(0, REPORT_CAP).join('');
  return `\n### CHILD_TO_PARENT id=${item.event_id} ts=${item.sent_at}\n${content}`;
}

function renderItem(item, seen) {
  return item.kind === 'report' ? renderReport(item) : renderMessage(item, seen);
}

// `items` are parent messages (kind 'message', with `files`) and child reports (kind 'report'), in any order.
export function buildChildPackets({ child, meta, parent, parentName, items }) {
  const head = headerLines({ child, meta, parent, parentName }).join('\n');
  const ordered = [...items].sort((a, b) => Date.parse(a.sent_at) - Date.parse(b.sent_at));
  const chunks = [];
  let current = { blocks: [], messageIds: [] };
  let seen = new Map();
  let size = length(head);
  for (const item of ordered) {
    let block = renderItem(item, seen);
    if (current.blocks.length > 0 && size + length(block) > PACKET_LIMIT) {
      chunks.push(current);
      current = { blocks: [], messageIds: [] };
      seen = new Map();
      size = length(head);
      block = renderItem(item, seen);
    }
    current.blocks.push(block);
    if (item.kind === 'message') current.messageIds.push(item.event_id);
    size += length(block);
  }
  chunks.push(current);
  return chunks.map((chunk, index) => ({
    name: `${child}__${index}`,
    child,
    parent,
    profile: meta.profile ?? null,
    text: `${head}\n${chunk.blocks.join('\n')}`,
    chars: length(head) + chunk.blocks.reduce((total, block) => total + length(block), 0),
    messageIds: chunk.messageIds,
  }));
}
