// The tally of one labeled window: counts per verdict, the share of content that is unneeded, and the groups the
// week-one review printed. The baseline column holds the week-one review's own numbers for the same window.

// sitrep_manager-rules-week1-review_v1.html, sections 3 and 4: the window 2026-10-14 20:24Z to 2026-10-21 20:30Z.
export const WEEK_ONE_BASELINE = {
  messages: 1104,
  children: 142,
  parents: 18,
  withUnneeded: 286,
  whollyUnrelated: 20,
};

function share(part, whole) {
  return whole === 0 ? 0 : (100 * part) / whole;
}

// One group's counts. Each message weighs its own text plus its merged file text, as the recovered tally did.
export function summarize(records) {
  const counts = { n: records.length, needed: 0, mixed: 0, unrelated: 0 };
  let weight = 0;
  let offWeight = 0;
  for (const record of records) {
    counts[record.verdict] += 1;
    const size = record.message_chars + record.file_chars;
    weight += size;
    offWeight += (size * record.off_goal_percent) / 100;
  }
  counts.withUnneeded = counts.mixed + counts.unrelated;
  counts.withUnneededPct = share(counts.withUnneeded, counts.n);
  counts.offGoalCharsPct = share(offWeight, weight);
  return counts;
}

function line(label, counts) {
  return (
    `${label.padEnd(36)} n=${String(counts.n).padStart(5)} needed=${String(counts.needed).padStart(4)} ` +
    `mixed=${String(counts.mixed).padStart(4)} unrelated=${String(counts.unrelated).padStart(3)} ` +
    `with_unneeded=${counts.withUnneededPct.toFixed(1).padStart(5)}% offgoal_chars=${counts.offGoalCharsPct.toFixed(1).padStart(5)}%`
  );
}

function groupBy(records, keyOf) {
  const groups = new Map();
  for (const record of records) {
    const key = keyOf(record);
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(record);
  }
  return groups;
}

export function formatTally({ run, records }) {
  const out = [];
  out.push(`window ${run.from} .. ${run.to}`);
  out.push(`judge ${run.judge_backend}; prompt sha256 manager ${run.prompt_sha256.manager.slice(0, 12)}, worker ${run.prompt_sha256.worker.slice(0, 12)}`);
  out.push('');
  const children = new Set(records.map((r) => r.target));
  const parents = new Set(records.map((r) => r.sender));
  const all = summarize(records);
  out.push(line('all', all));
  out.push(line('message has a merged file', summarize(records.filter((r) => r.has_file))));
  out.push(line('message has no merged file', summarize(records.filter((r) => !r.has_file))));
  const createdInWindow = records.filter((r) => Date.parse(r.target_created_at) >= Date.parse(run.from));
  out.push(line('child created in window', summarize(createdInWindow)));
  out.push(line('to manager children', summarize(records.filter((r) => r.target_profile === 'manager'))));
  out.push(line('to worker children', summarize(records.filter((r) => r.target_profile !== 'manager'))));
  out.push('by parent');
  const byParent = [...groupBy(records, (r) => r.sender).entries()].sort((a, b) => b[1].length - a[1].length);
  for (const [parent, group] of byParent) {
    out.push(line(`  ${parent.slice(0, 8)} ${String(group[0].sender_name ?? '').slice(0, 27)}`, summarize(group)));
  }
  const kinds = new Map();
  for (const record of records) {
    if (record.verdict === 'needed') continue;
    for (const kind of record.kinds) kinds.set(kind, (kinds.get(kind) ?? 0) + 1);
  }
  out.push(`kinds in mixed/unrelated ${JSON.stringify(Object.fromEntries([...kinds].sort()))}`);
  out.push('');
  out.push('against the week-one review (same window, same judging instructions)');
  out.push(`${''.padEnd(24)}${'rebuilt'.padEnd(24)}week-one review`);
  out.push(`${'messages'.padEnd(24)}${String(records.length).padEnd(24)}${WEEK_ONE_BASELINE.messages}`);
  out.push(`${'children'.padEnd(24)}${String(children.size).padEnd(24)}${WEEK_ONE_BASELINE.children}`);
  out.push(`${'parents'.padEnd(24)}${String(parents.size).padEnd(24)}${WEEK_ONE_BASELINE.parents}`);
  out.push(
    `${'with unneeded content'.padEnd(24)}${`${all.withUnneeded} (${all.withUnneededPct.toFixed(1)}%)`.padEnd(24)}` +
      `${WEEK_ONE_BASELINE.withUnneeded} (25.9%)`,
  );
  out.push(`${'wholly unrelated'.padEnd(24)}${String(all.unrelated).padEnd(24)}${WEEK_ONE_BASELINE.whollyUnrelated}`);
  out.push('');
  out.push('wholly unrelated messages');
  for (const record of records) {
    if (record.verdict !== 'unrelated') continue;
    out.push(`  ${record.sent_at.slice(0, 16)} ${record.sender.slice(0, 8)} -> ${record.target.slice(0, 8)} ${record.target_profile} off=${record.off_goal_percent} ${record.reason}`);
  }
  return `${out.join('\n')}\n`;
}
