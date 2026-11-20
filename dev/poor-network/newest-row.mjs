// The open scenario's newest-row rule: the last message of a first screen that the For you view paints. The bench
// reads the page's data-message-id of this message, so a message the view never paints cannot be the target.
//
// The For you filter (web/static/js/chat/for-you.js, showsInForYou) paints the roles in FOR_YOU_ROLES and the error
// rows of shared.js (a system message whose text starts with ERROR_PREFIX). A separator is folded into its reply's
// round buttons and is not painted; a message without an id has no node to name.
// Keep the two lists in step with those files.

const FOR_YOU_ROLES = ['user', 'web_reply', 'web_reply_notice', 'draft'];
const ERROR_PREFIX = 'Error: ';

export function paintsInForYou(msg) {
  if (FOR_YOU_ROLES.includes(msg.role)) return true;
  return msg.role === 'system' && String(msg.content || '').startsWith(ERROR_PREFIX);
}

// The newest painted message of a first screen, or null when the screen paints none.
export function newestRowMessage(messages) {
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i];
    if (msg.id != null && msg.id !== '' && paintsInForYou(msg)) return msg;
  }
  return null;
}

// The key of one repetition's target: its cell and its repetition number.
export function targetKey(cellId, repetition) {
  return `${cellId}#${repetition}`;
}

// The run-stable identity of a target: its role and its event index, the event's position in its session's log. A
// message's id is the id its event was stored under, and the server mints that id on each append, so two runs of one
// seed never share it. The seed's events are appended in the same order in every run, so the index is the same, and
// the page reads the same field (`event_index`, web/static/js/chat/for-you.js).
export function targetIdentity(msg) {
  if (!Number.isInteger(msg.event_index)) {
    throw new Error(`the newest-row message ${msg.id} carries no event_index`);
  }
  return `${msg.role}@${msg.event_index}`;
}

// The targets a before side measured, from its open result: one key per repetition of the reference cell and of each
// cell. A side that runs after it in one invocation reads them as the expectation its own targets are checked against.
export function expectedTargetsFrom(open) {
  const targets = new Map();
  for (const cell of [open.reference, ...open.cells]) {
    for (const rep of cell.reps) {
      targets.set(targetKey(cell.id, rep.rep), rep.newestTargetIdentity);
    }
  }
  return targets;
}

// Stops the side when its newest-row target differs from the before side's, so the two sides never measure reaching
// different messages. With no expectation (a single-side run) nothing is checked.
export function assertSameTargetAsBefore(expectedTargets, cellId, repetition, identity) {
  if (expectedTargets === null) return;
  const key = targetKey(cellId, repetition);
  if (!expectedTargets.has(key)) {
    throw new Error(`the before side measured no newest-row target for ${key}`);
  }
  const beforeIdentity = expectedTargets.get(key);
  if (beforeIdentity !== identity) {
    throw new Error(`the newest-row targets differ for ${key}: this side ${identity}, `
      + `the before side ${beforeIdentity}`);
  }
}
