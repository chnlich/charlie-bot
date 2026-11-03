// The offline replay of the pre-send check over the rebuilt labels.
//
// For each labeled message the replay runs the server's check (`charliebot presend-check replay`, which calls the Rust
// check function on the context as of the message), deletes every flagged passage, and judges the edited messages
// again with the review judge: each child's packet is rebuilt with its edited messages, and one judge call answers it.
// The check logic lives in Rust; this module holds no prompt text and no flag parsing.
//
// Output under `<out>/round<n>/`:
//   flags.jsonl       one row per labeled message: the check's backend, flags and time, or its failure cause
//   flip.jsonl        with --flip-test 1, the second check of each test-half message
//   rejudge.jsonl     one row per labeled message: the edited message's verdict, or why it has none
//   packets/          the edited packets each judge call read (message text: keep out of the repository)
//   judgments/        the parsed answers and raw result of each packet
//   run.json          the round's inputs and counts
//   tally.txt         the tally that `replay-tally` recomputes from these files

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';

import { judgePacket, promptKind, sha256 } from './label.mjs';
import { mergeFiles } from './files.mjs';
import { buildChildPackets } from './packets.mjs';
import { runPool } from './judge.mjs';
import { parentEventsOf, readSessionMetadata, reportsFrom, windowMessages } from './window.mjs';

// The tuning half is the messages sent before this instant (UTC), the test half the rest.
export const SPLIT_AT = '2026-10-18T00:00:00Z';
// The threshold of Trade-off 1: the test half's projected unneeded share must be under this, and its unrelated count 0.
export const THRESHOLD_SHARE = 0.1;

const UNNEEDED = ['mixed', 'unrelated'];

export function isUnneeded(verdict) {
  return UNNEEDED.includes(verdict);
}

export function halfOf(sentAt) {
  return Date.parse(sentAt) < Date.parse(SPLIT_AT) ? 'tuning' : 'test';
}

// The message text after the sender deletes every flagged passage: each flagged quote is cut out wherever it occurs.
export function deleteFlagged(content, flags) {
  let text = content;
  for (const flag of flags) text = text.split(flag.quote).join('');
  return text.trim();
}

// Two checks agree when their flag sets hold the same (kind, quote) pairs; the reasons do not count.
export function flagKey(flags) {
  return flags
    .map((flag) => JSON.stringify([flag.kind, flag.quote]))
    .sort()
    .join('\n');
}

// The median of an ascending list, or null for an empty one.
export function median(sorted) {
  const n = sorted.length;
  if (n === 0) return null;
  const mid = Math.floor(n / 2);
  return n % 2 === 1 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

// The nearest-rank percentile of an ascending list, or null for an empty one.
export function percentile(sorted, fraction) {
  if (sorted.length === 0) return null;
  return sorted[Math.max(0, Math.ceil(fraction * sorted.length) - 1)];
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function readJsonl(file) {
  return fs
    .readFileSync(file, 'utf8')
    .split('\n')
    .filter((line) => line !== '')
    .map((line) => JSON.parse(line));
}

function writeJsonl(file, rows) {
  fs.writeFileSync(file, rows.map((row) => `${JSON.stringify(row)}\n`).join(''));
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

// The check batch: one `presend-check replay` process holds one backend pool, so the light backends rotate as the
// server's do. Each input line names a message; the output has one row per line, in the same order.
export function parseBatch(stdout, items) {
  const rows = stdout
    .split('\n')
    .filter((line) => line !== '')
    .map((line) => JSON.parse(line));
  if (rows.length !== items.length) {
    throw new Error(`presend-check answered ${rows.length} of ${items.length} items`);
  }
  rows.forEach((row, index) => {
    if (row.child !== items[index].child || row.event !== items[index].event) {
      throw new Error(`presend-check row ${index + 1} names ${row.child}/${row.event}, not the item sent`);
    }
  });
  return rows;
}

export function runCheckBatch({ items, charliebot, home, repoRoot, parallel }) {
  return new Promise((resolve, reject) => {
    const env = { ...process.env, CHARLIEBOT_HOME: home, CHARLIEBOT_REPO_ROOT: repoRoot };
    const args = ['presend-check', 'replay', '--parallel', String(parallel)];
    const child = spawn(charliebot, args, { env, stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    child.on('error', (error) => reject(new Error(`cannot start ${charliebot}: ${error.message}`)));
    child.stdin.on('error', (error) => reject(new Error(`writing the check items: ${error.message}`)));
    child.on('close', (code) => {
      if (code !== 0) {
        reject(new Error(`presend-check exited with ${code}: ${stderr.slice(-400)}`));
        return;
      }
      try {
        resolve(parseBatch(stdout, items));
      } catch (error) {
        reject(error);
      }
    });
    child.stdin.end(items.map((item) => `${JSON.stringify({ child: item.child, event: item.event })}\n`).join(''));
  });
}

// The window's messages and the rebuilt labels must be the same set; a store that moved since the rebuild fails here.
function loadBaseline({ rebuild, home }) {
  const labels = readJsonl(path.join(rebuild, 'labels.jsonl'));
  const run = readJson(path.join(rebuild, 'run.json'));
  const sessionsRoot = path.join(home, 'sessions');
  const fromMs = Date.parse(run.from);
  const toMs = Date.parse(run.to);
  const reportsUntilMs = Date.parse(run.reports_until);
  const sessions = readSessionMetadata(sessionsRoot);
  const messages = windowMessages({ sessionsRoot, sessions, fromMs, toMs });
  const byEvent = new Map(messages.map((message) => [message.event_id, message]));
  if (messages.length !== labels.length) {
    throw new Error(`the window holds ${messages.length} messages now, the rebuilt labels ${labels.length}`);
  }
  for (const label of labels) {
    if (!byEvent.has(label.event_id)) throw new Error(`labeled message ${label.event_id} is not in the window now`);
  }
  return { labels, byEvent, sessions, sessionsRoot, fromMs, reportsUntilMs, rebuildRun: run };
}

// The fields every row of a message carries, from its rebuilt label.
function baseOf(label) {
  return {
    event_id: label.event_id,
    child: label.target,
    sender: label.sender,
    sent_at: label.sent_at,
    half: halfOf(label.sent_at),
    label_verdict: label.verdict,
  };
}

// One check's row: the message's base fields, then the backend, flags and time, or the cause of the failure.
function checkRow(base, row) {
  if (row.error !== undefined) {
    return { ...base, check: 'failed', backend: null, check_ms: null, flags: [], error: row.error };
  }
  return { ...base, check: 'ok', backend: row.backend, check_ms: row.check_ms, flags: row.flags, error: null };
}

// The edited message and its state: a check that failed is skipped, a message with no content left is not sent.
function editOf(check, content) {
  if (check.check === 'failed') return { ...check, state: 'check_failed', edited: null };
  const edited = deleteFlagged(content, check.flags);
  return { ...check, state: edited === '' ? 'not_sent' : 'sent', edited };
}

// One judge pass per child packet over the edited messages of that child, with the same packet layout as the labels.
async function rejudgeEdited({ baseline, rows, roundDir, model, prompts, judgeParallel }) {
  const sent = rows.filter((row) => row.state === 'sent');
  const packets = [];
  const parentEvents = new Map();
  for (const [child, group] of groupBy(sent, (row) => row.child)) {
    const meta = baseline.sessions.get(child);
    const parent = meta.task_parent_id;
    if (!parentEvents.has(parent)) {
      parentEvents.set(parent, parentEventsOf({ sessionsRoot: baseline.sessionsRoot, parent }));
    }
    const items = group.map((row) => ({
      kind: 'message',
      event_id: row.event_id,
      sent_at: row.sent_at,
      content: row.edited,
      files: mergeFiles({ text: row.edited, sender: parent, child, root: baseline.sessionsRoot, home: os.homedir() }),
    }));
    items.push(
      ...reportsFrom({
        parentEvents: parentEvents.get(parent),
        child,
        fromMs: baseline.fromMs,
        toMs: baseline.reportsUntilMs,
      }),
    );
    const parentName = baseline.sessions.get(parent).name ?? null;
    packets.push(...buildChildPackets({ child, meta, parent, parentName, items }));
  }
  for (const packet of packets) {
    fs.writeFileSync(path.join(roundDir, 'packets', `${packet.name}.md`), packet.text);
  }

  const answers = new Map();
  const causes = new Map();
  let failedPackets = 0;
  const tasks = packets.map((packet) => async () => {
    const kind = promptKind(packet.profile);
    try {
      const judged = await judgePacket({ packet, prompt: prompts[kind], model });
      fs.writeFileSync(
        path.join(roundDir, 'judgments', `${packet.name}.json`),
        `${JSON.stringify(
          {
            packet: packet.name,
            child: packet.child,
            judge_backend: model,
            prompt_kind: kind,
            prompt_sha256: sha256(prompts[kind]),
            answers: judged.answers,
            result: judged.result,
            fill_result: judged.fillResult,
            dropped_ids: judged.dropped,
          },
          null,
          1,
        )}\n`,
      );
      for (const answer of judged.answers) answers.set(answer.id, answer);
    } catch (error) {
      failedPackets += 1;
      process.stderr.write(`${packet.name}: ${error.message}\n`);
      for (const id of packet.messageIds) causes.set(id, `${packet.name}: ${error.message}`);
    }
  });
  await runPool(tasks, judgeParallel);
  return { packets, answers, causes, failedPackets };
}

export async function runReplayRound({
  out,
  rebuild,
  round,
  home,
  charliebot,
  repoRoot,
  checkParallel,
  judgeParallel,
  model,
  prompts,
  flipTest,
}) {
  const roundDir = path.join(out, `round${round}`);
  fs.mkdirSync(path.join(roundDir, 'packets'), { recursive: true });
  fs.mkdirSync(path.join(roundDir, 'judgments'), { recursive: true });
  const startedAt = new Date().toISOString();
  const promptPath = path.join(repoRoot, 'prompts', 'presend_check.md');
  const promptSha = sha256(fs.readFileSync(promptPath, 'utf8'));
  const baseline = loadBaseline({ rebuild, home });

  const items = baseline.labels.map((label) => ({ child: label.target, event: label.event_id }));
  const checkRows = await runCheckBatch({ items, charliebot, home, repoRoot, parallel: checkParallel });
  const rows = baseline.labels.map((label, index) => {
    const content = baseline.byEvent.get(label.event_id).content;
    return editOf(checkRow(baseOf(label), checkRows[index]), content);
  });
  writeJsonl(path.join(roundDir, 'flags.jsonl'), rows);

  let flipCount = 0;
  if (flipTest) {
    // The second check of each test-half message, on the same items; the flag sets of the two runs are compared.
    const testRows = rows.filter((row) => row.half === 'test');
    const flipItems = testRows.map((row) => ({ child: row.child, event: row.event_id }));
    const flipRows = await runCheckBatch({ items: flipItems, charliebot, home, repoRoot, parallel: checkParallel });
    flipCount = flipRows.length;
    const flipChecks = testRows.map((row, index) => {
      const { event_id, child, sender, sent_at, half, label_verdict } = row;
      return checkRow({ event_id, child, sender, sent_at, half, label_verdict }, flipRows[index]);
    });
    writeJsonl(path.join(roundDir, 'flip.jsonl'), flipChecks);
  }

  const judged = await rejudgeEdited({ baseline, rows, roundDir, model, prompts, judgeParallel });
  const rejudge = rows.map((row) => {
    const base = { event_id: row.event_id, child: row.child, half: row.half, state: row.state };
    if (row.state === 'check_failed') {
      const error = `check failed: ${row.error}`;
      return { ...base, after_verdict: null, after_kinds: null, after_reason: null, error };
    }
    if (row.state === 'not_sent') {
      return { ...base, after_verdict: null, after_kinds: null, after_reason: null, error: null };
    }
    const answer = judged.answers.get(row.event_id);
    if (answer) {
      return {
        ...base,
        after_verdict: answer.verdict,
        after_kinds: answer.off_goal_kinds,
        after_reason: answer.reason,
        error: null,
      };
    }
    const cause = judged.causes.get(row.event_id) ?? 'the judge gave no answer for this message';
    return { ...base, after_verdict: null, after_kinds: null, after_reason: null, error: `rejudge failed: ${cause}` };
  });
  writeJsonl(path.join(roundDir, 'rejudge.jsonl'), rejudge);

  const run = {
    round,
    started_at: startedAt,
    finished_at: new Date().toISOString(),
    prompt_sha256: promptSha,
    repo_root: repoRoot,
    charliebot,
    split_at: SPLIT_AT,
    window: {
      from: baseline.rebuildRun.from,
      to: baseline.rebuildRun.to,
      reports_until: baseline.rebuildRun.reports_until,
    },
    messages: rows.length,
    states: {
      sent: rows.filter((row) => row.state === 'sent').length,
      not_sent: rows.filter((row) => row.state === 'not_sent').length,
      check_failed: rows.filter((row) => row.state === 'check_failed').length,
    },
    check_items: items.length,
    flip_check_items: flipCount,
    judge_model: model,
    judge_packets: judged.packets.length,
    judge_packets_failed: judged.failedPackets,
    check_parallel: checkParallel,
    judge_parallel: judgeParallel,
  };
  fs.writeFileSync(path.join(roundDir, 'run.json'), `${JSON.stringify(run, null, 1)}\n`);
  return tallyRound(roundDir);
}

// The tally of one round, recomputed from its files: the numbers the decision page reports.
export function tallyRound(roundDir) {
  const run = readJson(path.join(roundDir, 'run.json'));
  const rows = readJsonl(path.join(roundDir, 'flags.jsonl'));
  const rejudge = new Map(readJsonl(path.join(roundDir, 'rejudge.jsonl')).map((row) => [row.event_id, row]));
  const flipFile = path.join(roundDir, 'flip.jsonl');
  const flip = fs.existsSync(flipFile) ? new Map(readJsonl(flipFile).map((row) => [row.event_id, row])) : null;

  const flagged = (row) => row.check === 'ok' && row.flags.length > 0;
  const bad = rows.filter((row) => isUnneeded(row.label_verdict));
  const good = rows.filter((row) => row.label_verdict === 'needed');
  const test = rows.filter((row) => row.half === 'test');
  const evaluated = test.filter((row) => typeof rejudge.get(row.event_id).after_verdict === 'string');
  const afterUnneeded = evaluated.filter((row) => isUnneeded(rejudge.get(row.event_id).after_verdict));
  const afterUnrelated = evaluated.filter((row) => rejudge.get(row.event_id).after_verdict === 'unrelated');
  const afterShare = evaluated.length === 0 ? null : afterUnneeded.length / evaluated.length;
  const testBad = test.filter((row) => isUnneeded(row.label_verdict));
  const testUnrelated = test.filter((row) => row.label_verdict === 'unrelated');

  // A failed check is a check failure; a sent message whose rejudge has a cause is a rejudge failure. A check-failed
  // message carries its skip in the rejudge row, which is not a second failure.
  const failures = [];
  for (const row of rows) {
    if (row.state === 'check_failed') failures.push({ half: row.half, stage: 'check', cause: row.error });
    const after = rejudge.get(row.event_id);
    if (row.state === 'sent' && after.error !== null) {
      failures.push({ half: row.half, stage: 'rejudge', cause: after.error });
    }
  }

  const times = rows
    .filter((row) => row.check === 'ok')
    .map((row) => row.check_ms)
    .sort((a, b) => a - b);

  let flipStats = null;
  if (flip) {
    const pairs = test.filter((row) => row.check === 'ok' && flip.get(row.event_id).check === 'ok');
    const differ = pairs.filter((row) => flagKey(row.flags) !== flagKey(flip.get(row.event_id).flags));
    flipStats = { pairs: pairs.length, differ: differ.length, items: flip.size };
  }

  const threshold =
    afterShare !== null && afterShare < THRESHOLD_SHARE && afterUnrelated.length === 0;
  return {
    round: run.round,
    prompt_sha256: run.prompt_sha256,
    all: {
      messages: rows.length,
      bad: bad.length,
      bad_flagged: bad.filter(flagged).length,
      good: good.length,
      good_flagged: good.filter(flagged).length,
    },
    test: {
      messages: test.length,
      baseline_unneeded: testBad.length,
      baseline_unrelated: testUnrelated.length,
      baseline_share: test.length === 0 ? null : testBad.length / test.length,
      not_sent: test.filter((row) => row.state === 'not_sent').length,
      after_judged: evaluated.length,
      after_unneeded: afterUnneeded.length,
      after_unrelated: afterUnrelated.length,
      after_share: afterShare,
      failed: failures.filter((failure) => failure.half === 'test').length,
      threshold_reached: threshold,
    },
    flip: flipStats,
    latency_ms: {
      n: times.length,
      median: median(times),
      p90: percentile(times, 0.9),
      max: times.length === 0 ? null : times[times.length - 1],
    },
    failures: {
      total: failures.length,
      by_half: failures.reduce((counts, failure) => {
        const key = `${failure.half} ${failure.stage}`;
        counts[key] = (counts[key] ?? 0) + 1;
        return counts;
      }, {}),
      causes: [...groupBy(failures, (failure) => `${failure.stage}: ${failure.cause}`)].map(([cause, group]) => ({
        cause,
        count: group.length,
      })),
    },
    states: run.states,
    calls: {
      check_items: run.check_items,
      flip_check_items: run.flip_check_items,
      judge_model: run.judge_model,
      judge_packets: run.judge_packets,
      judge_packets_failed: run.judge_packets_failed,
    },
  };
}

function percent(part, whole) {
  return whole === 0 || whole === null ? 'n/a' : `${((100 * part) / whole).toFixed(1)}%`;
}

export function formatTally(tally) {
  const { all, test, flip, latency_ms: latency, failures, calls } = tally;
  const share = (part, whole) => percent(part, whole);
  const lines = [
    `round ${tally.round}   prompt sha256 ${tally.prompt_sha256}`,
    '',
    '1. all labeled messages',
    `   bad (mixed or unrelated) with at least one flag: ${all.bad_flagged} of ${all.bad} ` +
      `(${share(all.bad_flagged, all.bad)})`,
    `   good (needed) with any flag: ${all.good_flagged} of ${all.good} (${share(all.good_flagged, all.good)})`,
    '',
    '2. test half (sent 10-18 to 10-21)',
    `   rebuilt baseline: ${test.baseline_unneeded} of ${test.messages} with unneeded content ` +
      `(${share(test.baseline_unneeded, test.messages)}), ${test.baseline_unrelated} wholly unrelated`,
    `   after deleting every flagged passage: ${test.after_unneeded} of ${test.after_judged} judged with unneeded ` +
      `content (${share(test.after_unneeded, test.after_judged)}), ${test.after_unrelated} wholly unrelated`,
    `   not sent (no content left): ${test.not_sent}; failed or skipped: ${test.failed}`,
    `   threshold (under 10% and 0 wholly unrelated): ${test.threshold_reached ? 'reached' : 'missed'}`,
    '',
    '3. flag flips (test half, checked twice)',
    flip
      ? `   ${flip.differ} of ${flip.pairs} with a different flag set (${share(flip.differ, flip.pairs)})`
      : '   not run in this round',
    '',
    `4. check latency per message (ms, ${latency.n} checks): median ${latency.median ?? 'n/a'}, ` +
      `p90 ${latency.p90 ?? 'n/a'}, max ${latency.max ?? 'n/a'}`,
    '',
    `5. failed or skipped: ${failures.total} of ${all.messages} messages`,
    ...failures.causes.map(({ cause, count }) => `   ${count} x ${cause}`),
    '',
    `calls: ${calls.check_items} check items, ${calls.flip_check_items} flip check items; judge model ` +
      `${calls.judge_model}, ${calls.judge_packets} packets (${calls.judge_packets_failed} failed)`,
  ];
  return `${lines.join('\n')}\n`;
}
