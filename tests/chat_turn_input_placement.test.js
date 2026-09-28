const assert = require('node:assert/strict');
const test = require('node:test');

const {
  loadChatContext,
  msg,
  separator,
  mountCase,
  wrappers,
  mountEngine,
  settle,
  engineDebug,
  assertEngineInvariants,
} = require('./turn_engine_harness');

// ---------------------------------------------------------------------------
// Turn-input placement: every input a session receives shows in the turn that
// handled it, outside the folded steps. A stimulus that arrives after a span's
// first assistant message was queued for a later round: the shared rule
// (shared.js splitTurnSpan) carries it out of the span and it opens the next
// one. These tests pin that placement through both consumers — the turn
// engine over message entries and the legacy DOM path over rendered elements —
// with expectations computed here from the message lists alone.
// ---------------------------------------------------------------------------

// --- fixtures ---------------------------------------------------------------
// Raw message JSON for the engine path; the ids double as the expectations'
// handles. `mid` marks a mid-round input (it arrives after the round's first
// assistant message).
function m(role, id, text, extra = {}) {
  return Object.assign(
      {role, id, content: text == null ? `${role} ${id}` : text, timestamp: '2026-04-02T10:07:00.000Z'},
      extra);
}

function sep(id, secs = 42) {
  return m('separator', id, '', {thinking_seconds: secs, event_index: 1000 + id.length});
}

// Sequence (a): a round answers A, an agent_message lands mid-round, the next
// round replies twice.
function sequenceA() {
  return [
    m('user', 'a', 'please switch the model and sweep the test sessions'),
    m('assistant', 's1', 'counting the sessions'),
    m('agent_message', 'mid1', 'relay: archive only the 12 e2e sessions'),
    m('assistant', 's2', 'archiving'),
    m('assistant', 'final', '12 test sessions archived'),
    sep('sep1'),
    m('assistant', 'r1', 'the archived set is listed'),
    m('assistant', 'r2', 'sweep complete, nothing else matched'),
    sep('sep2'),
  ];
}

// Sequence (b): one round consumes two inputs in a row.
function sequenceB() {
  return [
    m('user', 'a', 'run the sweep now'),
    m('agent_message', 'mid1', 'relay: also refresh the index'),
    m('assistant', 's1', 'sweeping'),
    m('assistant', 's2', 'refreshing'),
    m('assistant', 'final', 'sweep done, index refreshed'),
    sep('sep1'),
  ];
}

// --- independent expectations -----------------------------------------------
// The turn layout recomputed from the message list alone, mirroring the rule
// the suites pin: own inputs sit before the span's first assistant message,
// later stimuli carry to the next span, the head keeps the user-over-stimulus
// priority over the inputs, and the fold holds the work from the first
// assistant message to the conclusion (body[0] head and fold in a span with
// no inputs at all).
function expectedTurns(msgs) {
  const spans = [];
  let span = [];
  let carriedIn = [];
  for (const message of msgs) {
    span.push(message);
    if (message.role === 'separator') {
      spans.push({items: span});
      span = [];
    }
  }
  const turns = [];
  for (const oneSpan of spans) {
    // The span the rule sees: the carried-in inputs open it, ahead of its own
    // messages, and the trailing separator is no part of the rule.
    const held = [...carriedIn, ...oneSpan.items];
    const body = held.slice(0, -1);
    const conclusion = [...body].reverse().find((message) => message.role === 'assistant') || null;
    const firstAssistant = body.findIndex((message) => message.role === 'assistant');
    const assistantAt = firstAssistant === -1 ? body.length : firstAssistant;
    const ownInputs = body.filter(
        (message, i) => STIMULUS_ROLES.includes(message.role) && i < assistantAt);
    const carried = body.filter(
        (message, i) => STIMULUS_ROLES.includes(message.role) && i >= assistantAt);
    const head = [...ownInputs].reverse().find((message) => message.role === 'user')
      || ownInputs[ownInputs.length - 1]
      || body[0];
    if (!head) {
      carriedIn = carried;
      continue;
    }
    const conclusionAt = conclusion ? body.indexOf(conclusion) : body.length;
    let fold;
    if (!conclusion) {
      fold = [];
    } else if (ownInputs.length) {
      const queued = new Set(carried);
      fold = body.slice(assistantAt, conclusionAt).filter((message) => !queued.has(message));
    } else {
      fold = body.slice(1, conclusionAt);
    }
    turns.push({
      // The turn holds the carried-in inputs plus the span's own messages,
      // minus the stimuli queued for the next round.
      entries: held.filter((message) => !carried.includes(message)),
      head,
      conclusion,
      fold,
      carried,
    });
    carriedIn = carried;
  }
  return {turns, tail: span.concat(carriedIn)};
}

const STIMULUS_ROLES = ['user', 'scheduled_trigger', 'agent_message', 'worker_summary', 'child_report'];
const TYPE_LABELS = {user: 'You', scheduled_trigger: 'Trigger', agent_message: 'Agent', worker_summary: 'Worker', child_report: 'Report'};

function ids(messages) {
  return messages.map((message) => message.id);
}

// --- engine readers -----------------------------------------------------------
// The engine object lives in the harness's vm context, so its arrays carry
// that realm's Array prototype; spread them into test-realm arrays or
// deepStrictEqual rejects them on prototype identity alone.
function engineTurns(engine) {
  return [...engine.segments].filter((seg) => seg.kind === 'turn').map((seg) => ({
    key: seg.key,
    headRole: seg.rowSpec.headRole,
    headText: seg.rowSpec.headText,
    conclusionText: seg.rowSpec.conclusionText,
    foldIds: [...seg.foldEntries].map((entry) => entry.msg.id),
    entryIds: [...seg.entries].map((entry) => entry.msg.id),
  }));
}

function assertEngineMatchesExpected(engine, msgs, label) {
  const want = expectedTurns(msgs);
  const got = engineTurns(engine);
  assert.equal(got.length, want.turns.length, `${label}: turn count`);
  got.forEach((turn, i) => {
    const expected = want.turns[i];
    assert.equal(turn.headRole, expected.head.role, `${label}: turn ${i} head role`);
    assert.equal(turn.headText, expected.head.content, `${label}: turn ${i} head text`);
    assert.equal(turn.conclusionText,
        expected.conclusion ? expected.conclusion.content : null, `${label}: turn ${i} conclusion`);
    assert.deepEqual(turn.foldIds, ids(expected.fold), `${label}: turn ${i} fold`);
    assert.deepEqual(turn.entryIds, ids(expected.entries), `${label}: turn ${i} entries`);
    const separatorId = expected.entries[expected.entries.length - 1].id;
    const keyParts = [expected.head.id, expected.conclusion ? expected.conclusion.id : '', separatorId];
    assert.equal(turn.key, keyParts.join('|'), `${label}: turn ${i} key`);
  });
  // Every input lives in exactly one segment; the pending tail (if any) holds
  // exactly the last span's carried inputs.
  const pendingSegs = [...engine.segments].filter((seg) => seg.kind === 'flat' && seg.pending);
  const pendingIds = pendingSegs.flatMap((seg) => [...seg.entries].map((entry) => entry.msg.id));
  assert.deepEqual(pendingIds, ids(want.tail), `${label}: pending tail`);
}

// --- DOM readers --------------------------------------------------------------
function rowField(row, className) {
  const field = row.querySelector(`.${className}`);
  assert.ok(field, `row is missing .${className}`);
  return field.textContent;
}

function messageChildren(el) {
  return el.children.filter((child) => child.dataset && child.dataset.messageId);
}

function foldIdsOf(wrap) {
  const content = wrap.querySelector('.turn-fold-content');
  return content ? messageChildren(content).map((el) => el.dataset.messageId) : [];
}

function directIdsOf(wrap) {
  return messageChildren(wrap).map((el) => el.dataset.messageId);
}

function assertDomMatchesExpected(root, msgs, label) {
  const want = expectedTurns(msgs);
  const found = wrappers(root);
  assert.equal(found.length, want.turns.length, `${label}: wrapper count`);
  found.forEach((wrap, i) => {
    const expected = want.turns[i];
    const row = wrap.querySelector('.turn-row');
    assert.equal(rowField(row, 'turn-row-tag'), TYPE_LABELS[expected.head.role] || 'Turn',
        `${label}: turn ${i} tag`);
    assert.equal(rowField(row, 'turn-row-title'), expected.head.content, `${label}: turn ${i} title`);
    assert.equal(rowField(row, 'turn-row-conclusion'),
        expected.conclusion ? expected.conclusion.content : '', `${label}: turn ${i} conclusion line`);
    assert.deepEqual(foldIdsOf(wrap), ids(expected.fold), `${label}: turn ${i} fold`);
    // The turn's own inputs sit in the wrap, outside the fold band, ahead of
    // the work; the carried-in inputs open the wrap ahead of the span.
    const direct = directIdsOf(wrap);
    const expectedIds = ids(expected.entries);
    assert.deepEqual(direct.filter((id) => !ids(expected.fold).includes(id)).slice(0, 1),
        expectedIds.slice(0, 1), `${label}: turn ${i} first node`);
    ids(expected.entries).forEach((id) => {
      assert.ok(direct.includes(id) || foldIdsOf(wrap).includes(id),
          `${label}: turn ${i} entry ${id} left the wrapper`);
    });
    ownInputsOf(expected).forEach((id) => {
      assert.equal(foldIdsOf(wrap).includes(id), false, `${label}: turn ${i} input ${id} folded`);
    });
  });
  // Outside every wrapper exactly the pending stimuli remain, flat at
  // container level.
  const wrappedIds = new Set(want.turns.flatMap((turn) => ids(turn.entries)));
  const flat = messageChildren(root).map((el) => el.dataset.messageId)
      .filter((id) => !wrappedIds.has(id));
  assert.deepEqual(flat, ids(want.tail), `${label}: flat tail`);
}

function ownInputsOf(turn) {
  return ids(turn.entries.filter((message) => STIMULUS_ROLES.includes(message.role)));
}

// Sequence (a)'s item specs for the DOM path: buildElement reads text/ts off
// the item, so the raw message JSON is restated in that shape.
function messageItem(message) {
  if (message.role === 'separator') return separator(message.id, message.thinking_seconds);
  return msg(message.role, message.id, {text: message.content, ts: message.timestamp});
}

// --- (a) both paths -----------------------------------------------------------
test('(a) a mid-round input opens the turn that handles it, on both paths', () => {
  const msgs = sequenceA();

  const {context, root, timers, engine} = mountEngine(msgs);
  settle(timers);
  assertEngineMatchesExpected(engine, msgs, 'engine mount');
  assert.deepEqual([...engineDebug(context, root).keys], ['a|final|sep1', 'mid1|r2|sep2'],
      'engine mount: segment keys');
  assertEngineInvariants(context, root, root.children[root.children.length - 1], 'engine mount');

  const dom = mountCase(msgs.map((message) => messageItem(message)));
  dom.context.applyTurnOutline(dom.root);
  assertDomMatchesExpected(dom.root, msgs, 'dom mount');
});

// --- (b) both paths -----------------------------------------------------------
test('(b) one round consuming two inputs keeps both outside the fold', () => {
  const msgs = sequenceB();

  const {context, root, timers, engine} = mountEngine(msgs);
  settle(timers);
  assertEngineMatchesExpected(engine, msgs, 'engine mount');
  assertEngineInvariants(context, root, root.children[root.children.length - 1], 'engine mount');

  const dom = mountCase(msgs.map(messageItem));
  dom.context.applyTurnOutline(dom.root);
  assertDomMatchesExpected(dom.root, msgs, 'dom mount');

  // The head is the user ask even though the agent_message lands after it.
  const turn = engineTurns(engine)[0];
  assert.equal(turn.headRole, 'user');
  assert.equal(turn.headText, 'run the sweep now');
  assert.deepEqual(turn.foldIds, ['s1', 's2']);
});

// A notice between the last input and the first assistant message stays
// outside the fold with the inputs (the plan's model-switch boundary).
test('a notice between the last input and the first assistant message stays outside the fold', () => {
  const msgs = [
    m('user', 'a', 'the ask'),
    m('system', 'notice', 'model switched to the pinned one'),
    m('assistant', 's1', 'step one'),
    m('assistant', 's2', 'step two'),
    m('assistant', 'final', 'done'),
    sep('sep1'),
  ];
  const {context, root, timers, engine} = mountEngine(msgs);
  settle(timers);
  assertEngineMatchesExpected(engine, msgs, 'engine mount');
  assert.deepEqual(engineTurns(engine)[0].foldIds, ['s1', 's2']);
});

// --- (c) a carry with no handling round yet -----------------------------------
test('(c) a carried input with no later separator waits in the pending tail, rendered', () => {
  const msgs = [
    m('user', 'a', 'the ask'),
    m('assistant', 's1', 'working'),
    m('agent_message', 'mid1', 'relay: one more thing'),
    m('assistant', 'final', 'the answer'),
    sep('sep1'),
  ];
  const {context, root, timers, engine} = mountEngine(msgs);
  settle(timers);

  assertEngineMatchesExpected(engine, msgs, 'engine mount');
  const debug = engineDebug(context, root);
  assert.equal(debug.pending[debug.pending.length - 1], true, 'the carry stays pending');
  assertEngineInvariants(context, root, root.children[root.children.length - 1], 'engine mount');

  const dom = mountCase(msgs.map(messageItem));
  dom.context.applyTurnOutline(dom.root);
  assertDomMatchesExpected(dom.root, msgs, 'dom mount');
  // The carried node is a direct child of the container, right after the
  // wrapper — visible, owned by no turn yet.
  const wrap = wrappers(dom.root)[0];
  const carried = dom.root.children[dom.root.children.indexOf(wrap) + 1];
  assert.equal(carried.dataset.messageId, 'mid1', 'carried node sits after the wrapper');
});

// --- (d) live append equals one-shot mount ------------------------------------
test('(d) feeding sequence (a) message by message yields the one-shot mount segments', () => {
  const msgs = sequenceA();
  const {context, root, timers, engine} = mountEngine([]);
  msgs.forEach((message) => engine.appendMessage(Object.assign({}, message), false));
  settle(timers);

  assertEngineMatchesExpected(engine, msgs, 'live append');
  assertEngineInvariants(context, root, root.children[root.children.length - 1], 'live append');

  const mounted = mountEngine(msgs);
  settle(mounted.timers);
  assert.deepEqual(engineTurns(engine), engineTurns(mounted.engine), 'segments equal the mount');
});

// --- (e) pagination across turn 1's separator ---------------------------------
test('(e) a page boundary after turn 1 separator re-derives to the single mount', () => {
  const msgs = sequenceA();
  const boundary = msgs.findIndex((message) => message.id === 'sep1');
  const newerPage = msgs.slice(boundary + 1);          // [r1, r2, sep2]
  const olderPage = msgs.slice(0, boundary + 1);       // [a .. sep1]

  const {context, root, timers, engine} = mountEngine(newerPage);
  settle(timers);
  engine.prependMessages(olderPage);
  settle(timers);

  assertEngineMatchesExpected(engine, msgs, 'after prepend');
  assertEngineInvariants(context, root, root.children[root.children.length - 1], 'after prepend');

  const mounted = mountEngine(msgs);
  settle(mounted.timers);
  assert.deepEqual(engineTurns(engine), engineTurns(mounted.engine), 'segments equal the mount');
});

// --- (g) a carry cascading across settled segments -----------------------------
test('(g) carries cascade across settled segments when a page boundary precedes two rounds', () => {
  // Two adjacent rounds each receive one mid-round input, and the page
  // boundary sits before both. The boundary additionally cuts the third
  // round's span between its first assistant message and its own mid-round
  // input, so the mounted page had mis-counted that input as the round's own
  // (the plan's one-turn-early window). Prepending re-derives the third
  // settled segment — its first assistant now arrives from the page, so its
  // mid-round input becomes a carry — and that carry re-derives the fourth
  // settled segment in turn.
  const turn = (head, mid, tag) => [
    m('user', head, `ask ${head}`),
    m('assistant', `s-${tag}1`, 'step one'),
    m('agent_message', mid, `relay ${mid}`),
    m('assistant', `s-${tag}2`, 'step two'),
    m('assistant', `c-${tag}`, `done ${head}`),
    sep(`sep-${tag}`),
  ];
  const msgs = [
    ...turn('a1', 'mid1', 't1'),
    ...turn('b1', 'mid2', 't2'),
    ...turn('c1', 'mid3', 't3'),
    m('user', 'd1', 'ask d1'),
    m('assistant', 's-t41', 'step one'),
    m('assistant', 'c-t4', 'done d1'),
    sep('sep-t4'),
  ];
  // The mounted page starts at turn 3's mid-round input — after the span's
  // first assistant message. Everything earlier rides the prepended page.
  const boundary = msgs.findIndex((message) => message.id === 'mid3');
  const newerPage = msgs.slice(boundary);
  const olderPage = msgs.slice(0, boundary);

  const {context, root, timers, engine} = mountEngine(newerPage);
  settle(timers);
  const rederivesBefore = engine.stats.rederivesOfSettledTurns;
  engine.prependMessages(olderPage);
  settle(timers);

  assertEngineMatchesExpected(engine, msgs, 'after prepend');
  assert.ok(engine.stats.rederivesOfSettledTurns >= rederivesBefore + 2,
      'the carry re-derived both settled segments');
  assertEngineInvariants(context, root, root.children[root.children.length - 1], 'after prepend');

  const mounted = mountEngine(msgs);
  settle(mounted.timers);
  assert.deepEqual(engineTurns(engine), engineTurns(mounted.engine), 'segments equal the mount');
});

// --- (f) DOM parity with the engine -------------------------------------------
test('(f) the DOM path places head, fold and carried nodes exactly as the engine', () => {
  for (const [name, msgs] of [['(a)', sequenceA()], ['(b)', sequenceB()]]) {
    const engineState = mountEngine(msgs);
    settle(engineState.timers);
    const engineTurnList = engineTurns(engineState.engine);

    const dom = mountCase(msgs.map(messageItem));
    dom.context.applyTurnOutline(dom.root);
    const found = wrappers(dom.root);
    assert.equal(found.length, engineTurnList.length, `${name}: turn count`);

    found.forEach((wrap, i) => {
      const turn = engineTurnList[i];
      const row = wrap.querySelector('.turn-row');
      assert.equal(rowField(row, 'turn-row-tag'), TYPE_LABELS[turn.headRole] || 'Turn',
          `${name}: turn ${i} tag`);
      assert.equal(rowField(row, 'turn-row-title'), turn.headText, `${name}: turn ${i} title`);
      assert.deepEqual(foldIdsOf(wrap), turn.foldIds, `${name}: turn ${i} fold`);
      // Outside the fold band the wrap holds the remaining entries in order.
      const unfolded = [...turn.entryIds].filter((id) => !turn.foldIds.includes(id));
      assert.deepEqual(directIdsOf(wrap), unfolded, `${name}: turn ${i} node order`);
    });
  }
});

// The shared rule itself: stimulus roles come from the one list in shared.js,
// so an agent_message head reads as Agent through the row builder.
test('the shared rule exports one stimulus list both consumers read', () => {
  const {context} = loadChatContext({
    getElementById: () => null,
  });
  // The rule runs in the harness's vm context, so its return values carry that
  // realm's prototypes: compare field by field.
  const split = context.splitTurnSpan(
      [{role: 'agent_message', id: 1}, {role: 'assistant', id: 2}],
      (message) => message.role);
  assert.deepEqual([...split.ownInputs].map((message) => message.id), [1], 'own inputs');
  assert.deepEqual([...split.carried].map((message) => message.id), [], 'carried');
  assert.equal(split.head.id, 1, 'head');
  assert.equal(split.conclusion.id, 2, 'conclusion');
  assert.deepEqual([...split.fold].map((message) => message.id), [], 'fold');
});
