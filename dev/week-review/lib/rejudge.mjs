// The re-judge: a seeded sample of the labeled messages, each judged again by a model from another vendor through the
// internal gateway. The model reads the same packet the Opus label read, with the instructions and the single-id wording
// the recovered review used for a retry. Agreement with the Opus labels is printed for a person to read; the exit code
// records only whether every answer had the expected shape.

import fs from 'node:fs';
import path from 'node:path';

import { chatCompletion, listModels, readCredential } from './gateway.mjs';
import { VERDICTS, checkAnswers, judgeInput, parseAnswers, runPool } from './judge.mjs';
import { promptKind } from './label.mjs';

// A seeded generator (mulberry32): the same seed draws the same sample on any run.
function mulberry32(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// `count` distinct indices of 0..total-1, in ascending order.
export function sampleIndices(total, count, seed) {
  if (count > total) throw new Error(`cannot sample ${count} of ${total} messages`);
  const random = mulberry32(seed);
  const indices = Array.from({ length: total }, (_, index) => index);
  for (let i = 0; i < count; i += 1) {
    const j = i + Math.floor(random() * (total - i));
    [indices[i], indices[j]] = [indices[j], indices[i]];
  }
  return indices.slice(0, count).sort((a, b) => a - b);
}

function bad(verdict) {
  return verdict !== 'needed';
}

function confusion(rows) {
  const counts = new Map(VERDICTS.map((opus) => [opus, new Map(VERDICTS.map((model) => [model, 0]))]));
  for (const row of rows) counts.get(row.opus_verdict).set(row.verdict, counts.get(row.opus_verdict).get(row.verdict) + 1);
  const lines = [`${'Opus \\ model'.padEnd(14)}${VERDICTS.map((v) => v.padStart(11)).join('')}`];
  for (const opus of VERDICTS) {
    lines.push(`${opus.padEnd(14)}${VERDICTS.map((model) => String(counts.get(opus).get(model)).padStart(11)).join('')}`);
  }
  return lines.join('\n');
}

export function summarizeRejudge({ model, total, seed, rows }) {
  const n = rows.length;
  const same = rows.filter((row) => row.verdict === row.opus_verdict).length;
  const sameBad = rows.filter((row) => bad(row.verdict) === bad(row.opus_verdict)).length;
  const out = [];
  out.push(`rejudge ${model}: ${n} messages sampled from ${total} (seed ${seed})`);
  out.push(`verdict agreement with the Opus labels: ${same}/${n} (${((100 * same) / n).toFixed(1)}%)`);
  out.push(`needed vs unneeded agreement: ${sameBad}/${n} (${((100 * sameBad) / n).toFixed(1)}%)`);
  out.push('');
  out.push(confusion(rows));
  out.push('');
  out.push('disagreements (event id prefix, Opus verdict, model verdict, model reason)');
  for (const row of rows) {
    if (row.verdict === row.opus_verdict) continue;
    out.push(`  ${row.event_id.slice(0, 8)} ${row.opus_verdict} -> ${row.verdict} ${row.reason}`);
  }
  return `${out.join('\n')}\n`;
}

function readJsonl(file) {
  return fs
    .readFileSync(file, 'utf8')
    .split('\n')
    .filter((line) => line !== '')
    .map((line) => JSON.parse(line));
}

export async function rejudgeSample({ out, model, sample, seed, gateway, credentialsPath, credentialName, prompts, parallel }) {
  const messages = readJsonl(path.join(out, 'messages.jsonl'));
  const chosen = sampleIndices(messages.length, sample, seed).map((index) => messages[index]);
  const key = readCredential(credentialsPath, credentialName);
  const models = await listModels({ base: gateway, key });
  if (!models.includes(model)) throw new Error(`the gateway at ${gateway} lists no model ${model}`);

  const rows = new Array(chosen.length);
  await runPool(
    chosen.map((message, index) => async () => {
      const packetText = fs.readFileSync(path.join(out, 'packets', `${message.packet}.md`), 'utf8');
      const where = `rejudge ${message.event_id}`;
      const content = await chatCompletion({
        base: gateway,
        key,
        model,
        input: judgeInput({ prompt: prompts[promptKind(message.target_profile)], packetText, ids: [message.event_id] }),
        where,
      });
      const answers = parseAnswers(content, where);
      checkAnswers(answers, [message.event_id], where);
      const answer = answers[0];
      rows[index] = {
        event_id: message.event_id,
        sender: message.sender,
        target: message.target,
        sent_at: message.sent_at,
        verdict: answer.verdict,
        kinds: answer.off_goal_kinds,
        judge_backend: model,
        opus_verdict: message.verdict,
        reason: answer.reason,
      };
    }),
    parallel,
  );
  fs.writeFileSync(path.join(out, 'rejudge.jsonl'), rows.map((row) => `${JSON.stringify(row)}\n`).join(''));
  const text = summarizeRejudge({ model, total: messages.length, seed, rows });
  fs.writeFileSync(path.join(out, 'rejudge_summary.txt'), text);
  return text;
}
