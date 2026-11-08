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
// reply text into those 40 messages, so this size sets the bootstrap content's bytes near the live view's median,
// 26,592 bytes on the wire untrimmed. A reply of 8 KB makes about three or four stream updates.
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

// The tool-heavy piece: TOOL_PAIRS Bash call and result pairs, one call every TOOL_CALL_SPAN_SECONDS, then a short
// closing reply. A turn whose prompt carries the marker `bench-tools` prints it (the stub greps for the marker).
// Each line states its replay delay in `bench_delay_s`, the field the stub sleeps for; the reply pieces state none
// and keep the stub's default. The pair's shapes put one rendered tool row at ROW_BYTES: the command is the row's
// read field, so it and the output preview survive at their full lengths.
export const TOOL_PAIRS = 20;
export const TOOL_CALL_SPAN_SECONDS = 5;
export const TOOL_MARKER = 'bench-tools';
const TOOL_ROW_BYTES = 770;
// The fixed JSON around a row's command and output:
// {"name":"Bash","input":{"command":""},"output":"","is_error":false}
const TOOL_ROW_FIXED_BYTES = 67;
const TOOL_COMMAND_CHARS = 228;
const TOOL_OUTPUT_CHARS = TOOL_ROW_BYTES - TOOL_ROW_FIXED_BYTES - TOOL_COMMAND_CHARS;

function toolNdjson() {
  const lines = [];
  for (let index = 1; index <= TOOL_PAIRS; index++) {
    const command = `bench-tools call ${index} ${'x'.repeat(TOOL_COMMAND_CHARS)}`.slice(0, TOOL_COMMAND_CHARS);
    const output = `ok ${index} ${'y'.repeat(TOOL_OUTPUT_CHARS)}`.slice(0, TOOL_OUTPUT_CHARS);
    lines.push(`${JSON.stringify({
      type: 'command', step: index, id: `bench-tools-${index}`, command, bench_delay_s: TOOL_CALL_SPAN_SECONDS,
    })}\n`);
    lines.push(`${JSON.stringify({
      type: 'observation', step: index, id: `bench-tools-${index}`, returncode: 0, output, bench_delay_s: 0.05,
    })}\n`);
  }
  lines.push(`${JSON.stringify({
    type: 'thought', text: `All ${TOOL_PAIRS} tool calls finished.\n`, bench_delay_s: 1,
  })}\n`);
  return lines.join('');
}

// One final tool row as the server renders it, for the JSON to record and a seed to check against.
export function toolRowBytes() {
  const command = 'x'.repeat(TOOL_COMMAND_CHARS);
  const output = 'y'.repeat(TOOL_OUTPUT_CHARS);
  return Buffer.byteLength(JSON.stringify({name: 'Bash', input: {command}, output, is_error: false}));
}

// `node reply.mjs <repo root> <out dir>` writes the pieces as reply-<n>.ndjson, the tool piece as tools-0.ndjson,
// and `next`, the index of the piece the next turn prints. The stub advances `next` on each turn.
if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(new URL(import.meta.url).pathname)) {
  const [repoRoot, outDir] = process.argv.slice(2);
  if (!repoRoot || !outDir) {
    throw new Error('usage: node reply.mjs <repo root> <out dir>');
  }
  fs.mkdirSync(outDir, { recursive: true });
  loadReplyPieces(repoRoot).forEach((piece, index) => {
    fs.writeFileSync(path.join(outDir, `reply-${index}.ndjson`), replyNdjson(piece));
  });
  fs.writeFileSync(path.join(outDir, 'tools-0.ndjson'), toolNdjson());
  fs.writeFileSync(path.join(outDir, 'next'), '0\n');
}
