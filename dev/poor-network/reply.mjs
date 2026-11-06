// The scripted replies: REPLY_PIECES pieces of exactly REPLY_BYTES ASCII bytes each, cut in order from the text of
// REPLY_SOURCES, repository docs. A reply is repository text, never session content. The stub backend prints the pieces
// in turn, as the charlie-code NDJSON that the backend reads. Consecutive turns carry different text, so the gzip of a
// session's history keeps each reply's bytes instead of folding identical replies into one copy.

import fs from 'node:fs';
import path from 'node:path';

export const REPLY_SOURCES = Object.freeze([
  'dev/improve/landing.md',
  'docs/store-api.md',
  'docs/chat-platforms.md',
  'docs/registry.md',
  'docs/project.md',
  'rust/crates/runtime/tasks/README.md',
]);
export const REPLY_PIECES = 16;
export const REPLY_BYTES = 8192;
// The text of each streamed event. The server keeps each event as one message of the chat, and a bootstrap carries the
// last 40 messages, of which the turns' separators, notices and system rows take a share. Fewer, larger events put more
// reply text into those 40 messages, so this size sets the bootstrap's bytes near the live view's median, 26,592 bytes
// on the wire. A reply of 8 KB makes about three or four stream updates.
const EVENT_BYTES = 2200;

// The pieces of the reply text, each REPLY_BYTES long. Every non-ASCII character becomes a space, so a character is one
// byte and every piece has the same byte count.
export function loadReplyPieces(repoRoot) {
  const text = REPLY_SOURCES.map((file) => fs.readFileSync(path.join(repoRoot, file), 'utf8'))
    .join('\n')
    .replace(/[^\x20-\x7e\n]/g, ' ');
  if (text.length < REPLY_PIECES * REPLY_BYTES) {
    throw new Error(`the reply sources hold ${text.length} bytes, short of ${REPLY_PIECES * REPLY_BYTES}`);
  }
  return Array.from({ length: REPLY_PIECES }, (_, index) =>
    text.slice(index * REPLY_BYTES, (index + 1) * REPLY_BYTES),
  );
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

// `node reply.mjs <repo root> <out dir>` writes the pieces as reply-<n>.ndjson, and `next`, the index of the piece the
// next turn prints. The stub advances `next` on each turn.
if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(new URL(import.meta.url).pathname)) {
  const [repoRoot, outDir] = process.argv.slice(2);
  if (!repoRoot || !outDir) {
    throw new Error('usage: node reply.mjs <repo root> <out dir>');
  }
  fs.mkdirSync(outDir, { recursive: true });
  loadReplyPieces(repoRoot).forEach((piece, index) => {
    fs.writeFileSync(path.join(outDir, `reply-${index}.ndjson`), replyNdjson(piece));
  });
  fs.writeFileSync(path.join(outDir, 'next'), '0\n');
}
