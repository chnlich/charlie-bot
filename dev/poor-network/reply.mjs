// The scripted replies: REPLY_PIECES pieces of exactly REPLY_BYTES ASCII bytes each, cut in order from the fixed reply
// text in reply-fixture.txt. The fixture holds REPLY_SOURCES (repository docs) joined and ASCII-folded, so a reply is
// repository text, never session content, and a change to the docs cannot change what the bench sends. The stub backend
// prints the pieces in turn, as the charlie-code NDJSON that the backend reads. Consecutive turns carry different text,
// so the gzip of a session's history keeps each reply's bytes instead of folding identical replies into one copy.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

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
// The text of each streamed event of a stream, tool or open reply. The server keeps each event as one chat message.
// A reply of 8 KB makes about three or four stream updates.
const EVENT_BYTES = 2200;
// A sizing turn's reply: one SEED_PIECE_BYTES piece of the same text, one event. A sized session's untrimmed bootstrap
// is the gzip of its last messages, mostly events, so its size follows the event text. The bootstrap's window is its
// last 40 messages: a turn with a reply, a continuation run's acknowledgement and the turn-end checks' reasons and
// notices folds to about 11 messages, so a 40-message window held about 3.6 turns. At 8,250-byte pieces the window
// reached only 17,494 bytes, short of VIEW.bootstrapUntrimmedWireBytes (26,592). One 12,000-byte event a turn folds to
// 9 messages, so the window holds 4.4 turns, and the sized session reached 26,592 in its fourth turn. The reply pieces
// above keep their 8 KB and EVENT_BYTES.
export const SEED_PIECE_BYTES = 12000;
// The fixture holds 13 whole seed pieces (159,982 bytes): the sized rotation takes its pieces from these, not from the
// REPLY_PIECES, which need 16 × 8 KB.
export const SEED_PIECES = 13;
const SEED_EVENTS = 1;

// The fixed file holding the reply text: the REPLY_SOURCES files, joined with newlines, with every non-ASCII character
// replaced by spaces, copied byte for byte when this file was added. The bench reads this copy, so a change to the
// repository docs cannot change the reply the bench sends; the fixture's bytes are part of the bench's tree key.
const REPLY_FIXTURE = path.join(path.dirname(fileURLToPath(import.meta.url)), 'reply-fixture.txt');

// The reply text, one byte per character.
function replyText() {
  return fs.readFileSync(REPLY_FIXTURE, 'utf8');
}

// `count` pieces of `bytes` each, in order; every piece has the same byte count.
function slicePieces(text, bytes, count) {
  if (text.length < count * bytes) {
    throw new Error(`the reply fixture holds ${text.length} bytes, short of ${count * bytes}`);
  }
  return Array.from({ length: count }, (_, index) => text.slice(index * bytes, (index + 1) * bytes));
}

export function loadReplyPieces() {
  return slicePieces(replyText(), REPLY_BYTES, REPLY_PIECES);
}

// The charlie-code NDJSON of a list of events: one `thought` event per string.
function ndjsonOf(events) {
  return events.map((piece) => `${JSON.stringify({ type: 'thought', text: piece })}\n`).join('');
}

// A reply's events: runs of whole lines of about `eventBytes`.
function greedyEvents(text, eventBytes) {
  const events = [];
  let chunk = '';
  for (const line of text.split(/(?<=\n)/)) {
    if (chunk.length > 0 && Buffer.byteLength(chunk) + Buffer.byteLength(line) > eventBytes) {
      events.push(chunk);
      chunk = '';
    }
    chunk += line;
  }
  if (chunk.length > 0) {
    events.push(chunk);
  }
  return events;
}

// A sizing reply's events: `count` runs of whole lines, each cut where the text passes its share of the length.
function evenEvents(text, count) {
  const events = [];
  let chunk = '';
  let consumed = 0;
  for (const line of text.split(/(?<=\n)/)) {
    chunk += line;
    consumed += line.length;
    if (events.length < count - 1 && consumed >= ((events.length + 1) * text.length) / count) {
      events.push(chunk);
      chunk = '';
    }
  }
  if (chunk.length > 0) {
    events.push(chunk);
  }
  return events;
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

// `node reply.mjs <out dir>` writes the pieces as reply-<n>.ndjson, the same text as seed-<n>.ndjson for a sizing turn,
// the tool piece as tools-0.ndjson, and `next`, the index of the piece the next turn prints. The stub advances `next`
// on each turn.
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [outDir] = process.argv.slice(2);
  if (!outDir) {
    throw new Error('usage: node reply.mjs <out dir>');
  }
  fs.mkdirSync(outDir, { recursive: true });
  const text = replyText();
  slicePieces(text, REPLY_BYTES, REPLY_PIECES).forEach((piece, index) => {
    fs.writeFileSync(path.join(outDir, `reply-${index}.ndjson`), ndjsonOf(greedyEvents(piece, EVENT_BYTES)));
  });
  slicePieces(text, SEED_PIECE_BYTES, SEED_PIECES).forEach((piece, index) => {
    fs.writeFileSync(path.join(outDir, `seed-${index}.ndjson`), ndjsonOf(evenEvents(piece, SEED_EVENTS)));
  });
  fs.writeFileSync(path.join(outDir, 'tools-0.ndjson'), toolNdjson());
  fs.writeFileSync(path.join(outDir, 'next'), '0\n');
}
