// The scripted reply: about 8 KB of markdown cut from a repository file at a line boundary. The reply is repository
// text, never session content. The stub backend streams it as the charlie-code NDJSON that the backend reads.

import fs from 'node:fs';
import path from 'node:path';

export const REPLY_SOURCE = 'dev/improve/landing.md';
export const REPLY_BYTES = 8192;
// The text of each streamed event. The server turns each event into one stream update, so a reply of about 8 KB makes
// about 16 updates.
const EVENT_BYTES = 512;

// The longest run of whole lines from the start of the source file that stays within REPLY_BYTES.
export function loadReplyText(repoRoot) {
  const lines = fs.readFileSync(path.join(repoRoot, REPLY_SOURCE), 'utf8').split('\n');
  const kept = [];
  let bytes = 0;
  for (const line of lines) {
    const next = bytes + Buffer.byteLength(line) + 1;
    if (next > REPLY_BYTES) {
      break;
    }
    kept.push(line);
    bytes = next;
  }
  return kept.join('\n') + '\n';
}

// The charlie-code NDJSON of the reply: one `thought` event per run of whole lines of about EVENT_BYTES.
function replyNdjson(text) {
  const events = [];
  let chunk = '';
  for (const line of text.split(/(?<=\n)/)) {
    if (chunk.length > 0 && Buffer.byteLength(chunk) + Buffer.byteLength(line) > EVENT_BYTES) {
      events.push(chunk);
      chunk = '';
    }
    chunk += line;
  }
  if (chunk.length > 0) {
    events.push(chunk);
  }
  return events.map((piece) => `${JSON.stringify({ type: 'thought', text: piece })}\n`).join('');
}

// `node reply.mjs <repo root> <out file>` writes the stub's NDJSON for the repository's reply source.
if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(new URL(import.meta.url).pathname)) {
  const [repoRoot, outFile] = process.argv.slice(2);
  if (!repoRoot || !outFile) {
    throw new Error('usage: node reply.mjs <repo root> <out file>');
  }
  fs.writeFileSync(outFile, replyNdjson(loadReplyText(repoRoot)));
}
