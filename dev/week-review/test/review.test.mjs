// Contract tests for the week-one review tool. Every session, message and file here is synthetic and lives in a
// temporary directory that the test removes.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import { fileURLToPath } from 'node:url';

import { mergeFiles, FILE_CAP } from '../lib/files.mjs';
import { PACKET_LIMIT, buildChildPackets } from '../lib/packets.mjs';
import { VERDICTS, checkAnswers, parseAnswers, runPool, splitAnswers } from '../lib/judge.mjs';
import { readCredential } from '../lib/gateway.mjs';
import { sampleIndices } from '../lib/rejudge.mjs';
import { summarize } from '../lib/tally.mjs';
import { parentEventsOf, readSessionMetadata, reportsFrom, windowMessages } from '../lib/window.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
let scratch;

before(() => {
  scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'week-review-test-'));
});

after(() => {
  fs.rmSync(scratch, { recursive: true, force: true });
});

function writeFile(file, body) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, body);
}

function writeLog(file, events) {
  writeFile(file, `${events.map((event) => JSON.stringify(event)).join('\n')}\n`);
}

// A parent P with child C (worker) and a child D of another parent Q. C's log holds one message in the window, one
// after it, one from a session that is not the parent, and a line that does not parse.
function syntheticHome(name) {
  const root = path.join(scratch, name, 'sessions');
  writeFile(path.join(root, 'P', 'metadata.json'), JSON.stringify({ name: 'parent one', task_parent_id: null }));
  writeFile(path.join(root, 'Q', 'metadata.json'), JSON.stringify({ name: 'parent two', task_parent_id: null }));
  writeFile(
    path.join(root, 'C', 'metadata.json'),
    JSON.stringify({
      name: 'child',
      task_parent_id: 'P',
      profile: 'worker',
      created_at: '2026-01-01T00:00:00Z',
      task: { goal: 'do the thing', acceptance: ['it is done'], context_refs: ['plan.md'] },
    }),
  );
  writeFile(path.join(root, 'D', 'metadata.json'), JSON.stringify({ name: 'other child', task_parent_id: 'Q' }));
  writeLog(path.join(root, 'C', 'data', 'chat_events.jsonl'), [
    { type: 'agent_message', id: 'm1', from_session: 'P', timestamp: '2026-01-02T09:00:00.000Z', content: 'do it' },
    { type: 'agent_message', id: 'm2', from_session: 'P', timestamp: '2026-01-09T09:00:00.000Z', content: 'later' },
    { type: 'agent_message', id: 'm3', from_session: 'X', timestamp: '2026-01-02T09:30:00Z', content: 'not parent' },
    { type: 'user', id: 'u1', timestamp: '2026-01-02T09:45:00Z', content: 'not a message' },
  ]);
  fs.appendFileSync(path.join(root, 'C', 'data', 'chat_events.jsonl'), '{"type": "agent_message", "id": \n');
  writeLog(path.join(root, 'P', 'data', 'chat_events.jsonl'), [
    { type: 'agent_message', id: 'r1', from_session: 'C', timestamp: '2026-01-02T10:00:00.000Z', content: 'done' },
    { type: 'agent_message', id: 'r2', from_session: 'C', timestamp: '2026-01-05T10:00:00.000Z', content: 'after' },
  ]);
  writeLog(path.join(root, 'D', 'data', 'chat_events.jsonl'), [
    { type: 'agent_message', id: 'm9', from_session: 'Q', timestamp: '2026-01-02T09:00:00.000Z', content: 'other' },
  ]);
  return root;
}

const WINDOW = { fromMs: Date.parse('2026-01-01T00:00:00Z'), toMs: Date.parse('2026-01-03T00:00:00Z') };

test('a window selects the parent messages a child recorded in the window, and the child reports in it', () => {
  const sessionsRoot = syntheticHome('select');
  const sessions = readSessionMetadata(sessionsRoot);
  const messages = windowMessages({ sessionsRoot, sessions, ...WINDOW });
  assert.deepEqual(
    messages.map((message) => [message.event_id, message.sender, message.target, message.content]),
    [
      ['m1', 'P', 'C', 'do it'],
      ['m9', 'Q', 'D', 'other'],
    ],
  );
  const reports = reportsFrom({
    parentEvents: parentEventsOf({ sessionsRoot, parent: 'P' }),
    child: 'C',
    ...WINDOW,
  });
  assert.deepEqual(reports.map((report) => report.event_id), ['r1']);
});

test('merged files take one state each: text, own, missing, directory, binary and truncated', () => {
  const home = path.join(scratch, 'merge');
  const root = path.join(home, 'sessions');
  writeFile(path.join(home, 'notes', 'a.txt'), 'alpha\r\nbeta');
  writeFile(path.join(home, 'bin.dat'), Buffer.from([0xff, 0xfe, 0x00]));
  writeFile(path.join(home, 'big.txt'), 'x'.repeat(FILE_CAP + 5));
  writeFile(path.join(root, 'P', 'artifacts', 'plan.html'), '<html><style>p{}</style><body><p>Hello</p></body></html>');
  writeFile(path.join(root, 'C', 'artifacts', 'own.md'), 'the child wrote this');
  const text = [
    'read ~/notes/a.txt, then ~/notes/a.txt again.',
    'page artifacts/plan.html; own ~/sessions/C/artifacts/own.md; absent artifacts/own.md.',
    'binary ~/bin.dat, directory ~/notes, missing ~/gone.txt, long ~/big.txt.',
  ].join(' ');
  const files = mergeFiles({ text, sender: 'P', child: 'C', root, home });
  const byPath = new Map(files.map((file) => [file.path, file]));
  assert.equal(files.length, 8, 'a repeated path is merged once');
  assert.equal(byPath.get('~/notes/a.txt').state, 'read');
  assert.equal(byPath.get('~/notes/a.txt').text, 'alpha\nbeta');
  assert.equal(byPath.get('artifacts/plan.html').state, 'read');
  assert.ok(byPath.get('artifacts/plan.html').text.includes('Hello'));
  assert.ok(!byPath.get('artifacts/plan.html').text.includes('p{}'), 'style text is not shown');
  assert.equal(byPath.get('~/sessions/C/artifacts/own.md').state, 'own', 'a file in the child session shows its name');
  assert.equal(byPath.get('artifacts/own.md').state, 'missing', 'an artifacts path resolves in the parent session only');
  assert.equal(byPath.get('~/bin.dat').state, 'binary');
  assert.equal(byPath.get('~/notes').state, 'directory');
  assert.equal(byPath.get('~/gone.txt').state, 'missing');
  assert.equal(byPath.get('~/big.txt').state, 'truncated');
  assert.equal(byPath.get('~/big.txt').chars, FILE_CAP + 5);
  assert.equal(byPath.get('~/big.txt').text.length, FILE_CAP);
});

test('a packet shows a repeated file once and splits before the character limit', () => {
  const meta = { name: 'child', profile: 'worker', task: { goal: 'g', acceptance: [], context_refs: [] } };
  const file = { path: '~/a.txt', state: 'read', chars: 5, text: 'AAAAA' };
  const long = 'L'.repeat(PACKET_LIMIT / 2 + 1000);
  const items = [
    { kind: 'message', event_id: 'm1', sent_at: '2026-01-02T09:00:00Z', content: 'first', files: [file] },
    { kind: 'message', event_id: 'm2', sent_at: '2026-01-02T09:05:00Z', content: 'second', files: [file] },
    { kind: 'message', event_id: 'm3', sent_at: '2026-01-02T09:10:00Z', content: long, files: [] },
    { kind: 'message', event_id: 'm4', sent_at: '2026-01-02T09:15:00Z', content: long, files: [] },
    { kind: 'report', event_id: 'r1', sent_at: '2026-01-02T09:20:00Z', content: 'reported' },
  ];
  const packets = buildChildPackets({ child: 'C', meta, parent: 'P', parentName: 'parent one', items });
  assert.equal(packets.length, 2);
  assert.deepEqual(packets.map((packet) => packet.name), ['C__0', 'C__1']);
  assert.deepEqual(packets.flatMap((packet) => packet.messageIds), ['m1', 'm2', 'm3', 'm4']);
  const first = packets[0].text;
  assert.equal(first.split('AAAAA').length - 1, 1, 'the file text appears once');
  assert.ok(first.includes('same content as in message m1 above'), 'the second message points back to the first');
  assert.ok(packets[1].text.includes('reported'), 'a child report sits in the packet of its place in time');
});

test('an answer is the last fenced block, and a missing or malformed answer fails the check', () => {
  const corrected = [
    'Here is my answer.',
    '```json',
    '[{"id":"a"}]',
    '```',
    'I need to correct it.',
    '```json',
    '[{"id":"b"}]',
    '```',
  ].join('\n');
  assert.deepEqual(parseAnswers(corrected, 'test'), [{ id: 'b' }]);
  assert.deepEqual(parseAnswers('Result: [{"id":"a"}] done.', 'test'), [{ id: 'a' }]);

  const good = {
    id: 'a',
    verdict: 'mixed',
    off_goal_percent: 30,
    off_goal_kinds: ['background'],
    off_goal_quote: 'quoted',
    reason: 'why',
  };
  assert.equal(checkAnswers([good], ['a'], 'test').length, 1);
  assert.throws(() => checkAnswers([{ ...good, verdict: 'maybe' }], ['a'], 'test'), /verdict/);
  assert.throws(() => checkAnswers([{ ...good, off_goal_kinds: ['gossip'] }], ['a'], 'test'), /off_goal_kinds/);
  assert.throws(() => checkAnswers([], ['a'], 'test'), /no answer/);
  const split = splitAnswers([good, { ...good, id: 'x' }], ['a', 'b']);
  assert.deepEqual(split.kept.map((answer) => answer.id), ['a']);
  assert.deepEqual(split.dropped, ['x']);
  assert.throws(() => checkAnswers([good], ['b'], 'test'), /unknown id/);
  assert.deepEqual(VERDICTS, ['needed', 'mixed', 'unrelated']);
});

test('the gateway key is read from its own entry of the credentials file', () => {
  const file = path.join(scratch, 'credentials.yaml');
  writeFile(
    file,
    ['aigw:', '  api_key: "personal-key"', 'aigw_charliebot_agents:', '  api_key: team-key', 'slack:', '  token: x', ''].join('\n'),
  );
  assert.equal(readCredential(file, 'aigw_charliebot_agents'), 'team-key');
  assert.equal(readCredential(file, 'aigw'), 'personal-key');
  assert.throws(() => readCredential(file, 'absent'), /has no entry absent/);
});

test('the tally counts needed, mixed and unrelated, and weighs unneeded text by its share', () => {
  const records = [
    { verdict: 'needed', message_chars: 100, file_chars: 0, off_goal_percent: 0 },
    { verdict: 'mixed', message_chars: 100, file_chars: 100, off_goal_percent: 50 },
    { verdict: 'unrelated', message_chars: 50, file_chars: 0, off_goal_percent: 100 },
    { verdict: 'needed', message_chars: 50, file_chars: 0, off_goal_percent: 0 },
  ];
  const counts = summarize(records);
  assert.equal(counts.n, 4);
  assert.equal(counts.needed, 2);
  assert.equal(counts.mixed, 1);
  assert.equal(counts.unrelated, 1);
  assert.equal(counts.withUnneeded, 2);
  assert.equal(counts.withUnneededPct, 50);
  assert.equal(counts.offGoalCharsPct, 37.5);
});

test('a seeded sample is the same on every run, and holds distinct messages', () => {
  const first = sampleIndices(50, 20, 1);
  assert.deepEqual(sampleIndices(50, 20, 1), first);
  assert.equal(new Set(first).size, 20);
  assert.ok(first.every((index) => index >= 0 && index < 50));
  assert.throws(() => sampleIndices(5, 6, 1), /cannot sample/);
});

test('a pool rejects with the first failure while every in-flight task settles', async () => {
  let ran = false;
  await assert.rejects(
    runPool(
      [
        () => Promise.reject(new Error('first failure')),
        () => new Promise((resolve) => setTimeout(() => resolve((ran = true)), 10)),
      ],
      2,
    ),
    /first failure/,
  );
  assert.equal(ran, true, 'the task already in flight still ran to its end');
});
