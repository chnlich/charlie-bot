// The label run: the window's messages, their packets, one judgment per packet, and the rows the tally reads.
//
// Output under `out/`:
//   packets/<name>.md       the packet each judgment read (message text and merged files: keep out of the repository)
//   judgments/<name>.json   the parsed answers and the raw result text of each packet
//   messages.jsonl          one record per message, with the file states and the judge's answer
//   labels.jsonl            one row per message: event_id, sender, target, sent_at, verdict, kinds, judge_backend
//   run.json                the window, the judge, and the prompt hashes that produced the rows
//   tally.txt               the tally the `tally` command prints

import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { gatewayCall, resolveCall } from './backend.mjs';
import { chatCompletion } from './gateway.mjs';
import { buildChildPackets } from './packets.mjs';
import { isMerged, mergeFiles } from './files.mjs';
import { checkAnswers, judgeInput, parseAnswers, runPool, splitAnswers } from './judge.mjs';
import { formatTally } from './tally.mjs';
import { parentEventsOf, readSessionMetadata, reportsFrom, windowMessages } from './window.mjs';

export function sha256(text) {
  return crypto.createHash('sha256').update(text).digest('hex');
}

// A worker child takes the worker instructions; every other child takes the manager instructions.
export function promptKind(profile) {
  return profile === 'worker' ? 'worker' : 'manager';
}

function charCount(text) {
  return Array.from(text).length;
}

function groupBy(items, keyOf) {
  const groups = new Map();
  for (const item of items) {
    const key = keyOf(item);
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(item);
  }
  return groups;
}

function writeJsonl(file, rows) {
  fs.writeFileSync(file, rows.map((row) => `${JSON.stringify(row)}\n`).join(''));
}

// One judge pass over a packet. A message the pass left unanswered is judged once more, alone, with the same
// instructions; the retry keeps only the answers for those messages, as the recovered review's fill step did.
async function judgePacket({ packet, prompt, judge }) {
  const where = packet.name;
  const ask = (input, callWhere) =>
    chatCompletion({ base: judge.base, key: judge.key, model: judge.model, input, where: callWhere });
  const result = await ask(judgeInput({ prompt, packetText: packet.text }), where);
  const first = parseAnswers(result, where);
  if (!Array.isArray(first)) throw new Error(`${where}: the answer is not an array`);
  const kept = splitAnswers(first, packet.messageIds);
  const dropped = [...kept.dropped];
  let answers = kept.kept;
  const answered = new Set(answers.map((answer) => answer.id));
  const missing = packet.messageIds.filter((id) => !answered.has(id));
  let fillResult = null;
  if (missing.length > 0) {
    fillResult = await ask(judgeInput({ prompt, packetText: packet.text, ids: missing }), `${where} (fill)`);
    const filled = parseAnswers(fillResult, `${where} (fill)`);
    if (!Array.isArray(filled)) throw new Error(`${where} (fill): the answer is not an array`);
    const retry = splitAnswers(filled, missing);
    dropped.push(...retry.dropped);
    answers = [...answers, ...retry.kept];
  }
  if (dropped.length > 0) {
    process.stderr.write(`${where}: dropped ${dropped.length} answer(s) for ids outside the packet: ${dropped.join(', ')}\n`);
  }
  checkAnswers(answers, packet.messageIds, where);
  return { answers, result, fillResult, dropped };
}

// A judgment saved by an earlier run of the same window and judge option is reused; another option is refused.
function loadJudgment(file, packet, judgeId) {
  const saved = JSON.parse(fs.readFileSync(file, 'utf8'));
  if (saved.judge_backend !== judgeId) {
    throw new Error(
      `${packet.name}: saved judgment came from ${saved.judge_backend}, not ${judgeId}; use a fresh output directory`,
    );
  }
  checkAnswers(saved.answers, packet.messageIds, packet.name);
  return saved;
}

// The window's labels. The judge is the option `backends.calls.judge` selects, resolved before the store is read.
export async function labelWindow({ home, from, to, reportsUntil, out, credentialsPath, parallel, prompts }) {
  const sessionsRoot = path.join(home, 'sessions');
  if (!fs.existsSync(sessionsRoot)) throw new Error(`no session store at ${sessionsRoot}`);
  const fromMs = Date.parse(from);
  const toMs = Date.parse(to);
  const reportsUntilMs = Date.parse(reportsUntil);
  for (const [flag, value] of [['--from', fromMs], ['--to', toMs], ['--reports-until', reportsUntilMs]]) {
    if (Number.isNaN(value)) throw new Error(`${flag} is not a timestamp`);
  }
  const judge = gatewayCall(resolveCall('judge'), credentialsPath);

  const promptHashes = { manager: sha256(prompts.manager), worker: sha256(prompts.worker) };
  fs.mkdirSync(path.join(out, 'packets'), { recursive: true });
  const judgmentsDir = path.join(out, 'judgments');
  fs.mkdirSync(judgmentsDir, { recursive: true });
  const judgmentFiles = fs.readdirSync(judgmentsDir).filter((name) => name.endsWith('.json'));
  const runFile = path.join(out, 'run.json');
  let runMatchesJudge = false;
  if (fs.existsSync(runFile)) {
    const savedRun = JSON.parse(fs.readFileSync(runFile, 'utf8'));
    runMatchesJudge = savedRun.judge_backend === judge.id && savedRun.judge_model === judge.model;
    if (judgmentFiles.length > 0 && !runMatchesJudge) {
      throw new Error(
        `saved judgments came from ${savedRun.judge_backend} (${savedRun.judge_model}), not ` +
          `${judge.id} (${judge.model}); use a fresh output directory`,
      );
    }
  } else if (judgmentFiles.length > 0) {
    throw new Error('saved judgments have no run metadata; use a fresh output directory');
  }
  const runIdentity = {
    from,
    to,
    reports_until: reportsUntil,
    judge_backend: judge.id,
    judge_model: judge.model,
    prompt_sha256: promptHashes,
  };
  const homeDir = os.homedir();

  const sessions = readSessionMetadata(sessionsRoot);
  const messages = windowMessages({ sessionsRoot, sessions, fromMs, toMs });
  if (messages.length === 0) throw new Error('the window holds no parent-to-child messages');

  const parentEvents = new Map();
  const packets = [];
  for (const [child, group] of groupBy(messages, (message) => message.target)) {
    const meta = sessions.get(child);
    const parent = meta.task_parent_id;
    if (!parentEvents.has(parent)) parentEvents.set(parent, parentEventsOf({ sessionsRoot, parent }));
    for (const message of group) {
      message.files = mergeFiles({ text: message.content, sender: parent, child, root: sessionsRoot, home: homeDir });
    }
    const items = group.map((message) => ({
      kind: 'message',
      event_id: message.event_id,
      sent_at: message.sent_at,
      content: message.content,
      files: message.files,
    }));
    items.push(...reportsFrom({ parentEvents: parentEvents.get(parent), child, fromMs, toMs: reportsUntilMs }));
    const childPackets = buildChildPackets({
      child,
      meta,
      parent,
      parentName: sessions.get(parent).name ?? null,
      items,
    });
    // One event can reach several children, so a message's packet is recorded on the message itself, within its
    // child's group; a map keyed by event id alone would point at the last child's packet.
    const messageById = new Map(group.map((message) => [message.event_id, message]));
    for (const packet of childPackets) {
      packets.push(packet);
      for (const id of packet.messageIds) messageById.get(id).packet = packet.name;
    }
  }
  for (const packet of packets) {
    fs.writeFileSync(path.join(out, 'packets', `${packet.name}.md`), packet.text);
  }
  if (!runMatchesJudge) fs.writeFileSync(runFile, `${JSON.stringify(runIdentity, null, 1)}\n`);

  const judgments = new Map();
  const tasks = packets.map((packet) => async () => {
    const file = path.join(judgmentsDir, `${packet.name}.json`);
    const kind = promptKind(packet.profile);
    if (fs.existsSync(file)) {
      judgments.set(packet.name, loadJudgment(file, packet, judge.id));
      return;
    }
    const judged = await judgePacket({ packet, prompt: prompts[kind], judge });
    const saved = {
      packet: packet.name,
      child: packet.child,
      profile: packet.profile,
      judge_backend: judge.id,
      prompt_kind: kind,
      prompt_sha256: promptHashes[kind],
      answers: judged.answers,
      result: judged.result,
      fill_result: judged.fillResult,
      dropped_ids: judged.dropped,
    };
    fs.writeFileSync(file, `${JSON.stringify(saved, null, 1)}\n`);
    judgments.set(packet.name, saved);
  });
  await runPool(tasks, parallel);

  const answerMaps = new Map(
    [...judgments].map(([name, saved]) => [name, new Map(saved.answers.map((item) => [item.id, item]))]),
  );
  const records = messages.map((message) => {
    const answer = answerMaps.get(message.packet).get(message.event_id);
    const files = message.files;
    return {
      event_id: message.event_id,
      sender: message.sender,
      sender_name: message.sender_name,
      target: message.target,
      target_profile: message.target_meta.profile ?? null,
      target_created_at: message.target_meta.created_at ?? null,
      sent_at: message.sent_at,
      packet: message.packet,
      message_chars: charCount(message.content),
      files: files.map(({ path: filePath, state, chars }) => ({ path: filePath, state, chars: chars ?? null })),
      has_file: files.some(isMerged),
      file_chars: files.filter(isMerged).reduce((total, file) => total + charCount(file.text), 0),
      verdict: answer.verdict,
      kinds: answer.off_goal_kinds,
      off_goal_percent: answer.off_goal_percent,
      off_goal_quote: answer.off_goal_quote,
      reason: answer.reason,
      judge_backend: judge.id,
    };
  });
  writeJsonl(path.join(out, 'messages.jsonl'), records);
  writeJsonl(
    path.join(out, 'labels.jsonl'),
    records.map((record) => ({
      event_id: record.event_id,
      sender: record.sender,
      target: record.target,
      sent_at: record.sent_at,
      verdict: record.verdict,
      kinds: record.kinds,
      judge_backend: record.judge_backend,
    })),
  );
  const run = {
    from,
    to,
    reports_until: reportsUntil,
    judge_backend: judge.id,
    judge_model: judge.model,
    prompt_sha256: promptHashes,
    messages: records.length,
    children: new Set(records.map((record) => record.target)).size,
    parents: new Set(records.map((record) => record.sender)).size,
    packets: packets.length,
    fill_passes: [...judgments.values()].filter((saved) => saved.fill_result !== null).length,
    dropped_answers: [...judgments.values()].reduce((total, saved) => total + saved.dropped_ids.length, 0),
  };
  fs.writeFileSync(runFile, `${JSON.stringify(run, null, 1)}\n`);
  return { run, records };
}

export function writeTally(out, run, records) {
  const text = formatTally({ run, records });
  fs.writeFileSync(path.join(out, 'tally.txt'), text);
  return text;
}
