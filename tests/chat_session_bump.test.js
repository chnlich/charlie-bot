const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const CHAT_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat.js'),
  'utf8'
);

const ELEMENT_NODE = 1;
const TEXT_NODE = 3;

class FakeClassList {
  constructor(initial = '') {
    this._classes = new Set(String(initial).split(/\s+/).filter(Boolean));
  }

  add(...classes) {
    for (const className of classes) this._classes.add(className);
  }

  remove(...classes) {
    for (const className of classes) this._classes.delete(className);
  }

  contains(className) {
    return this._classes.has(className);
  }

  toggle(className, force) {
    if (force === undefined) {
      if (this._classes.has(className)) {
        this._classes.delete(className);
        return false;
      }
      this._classes.add(className);
      return true;
    }
    if (force) this._classes.add(className);
    else this._classes.delete(className);
    return !!force;
  }

  toString() {
    return Array.from(this._classes).join(' ');
  }
}

function escapeForFakeDom(str) {
  return String(str).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

// Height model behind FakeElement.getBoundingClientRect — see the getter.
const FAKE_LEAF_HEIGHT = 24;

function fakeElementHeight(el) {
  const styled = typeof el.style?.height === 'string' && el.style.height.endsWith('px');
  if (styled) return parseFloat(el.style.height);
  if (el.classList.contains('hidden')) return 0;
  let total = el.__baseHeight || 0;
  for (const child of el.children) total += fakeElementHeight(child);
  if (total === 0 && el.children.length === 0) return FAKE_LEAF_HEIGHT;
  return total;
}

class FakeText {
  constructor(text) {
    this.nodeType = TEXT_NODE;
    this.textContent = String(text);
    this.parentElement = null;
    this.parentNode = null;
  }
}

// Child bookkeeping runs over `_nodes`, which holds elements and text nodes
// alike: the fold row reads a bubble's text back off the rendered nodes, and
// that read only means anything if element boundaries are real here too.
class FakeElement {
  constructor(tagName = 'DIV', {id = '', className = ''} = {}) {
    this.nodeType = ELEMENT_NODE;
    this.tagName = String(tagName).toUpperCase();
    this.id = id;
    this.dataset = {};
    this.attributes = new Map();
    this.parentElement = null;
    this.parentNode = null;
    this._nodes = [];
    this.classList = new FakeClassList(className);
    this._className = className;
    this.innerHTML = '';
    this.scrollTop = 0;
    this.clientHeight = 0;
    this.style = {};
    this._listeners = {};
  }

  // Like a real scroller, the scrollable range derives from laid-out content.
  get scrollHeight() {
    return this._scrollHeight != null ? this._scrollHeight : fakeElementHeight(this);
  }

  set scrollHeight(value) {
    this._scrollHeight = value;
  }

  get className() {
    return this.classList.toString();
  }

  set className(value) {
    this._className = String(value || '');
    this.classList = new FakeClassList(this._className);
  }

  get childNodes() {
    return this._nodes;
  }

  get children() {
    return this._nodes.filter((node) => node.nodeType === ELEMENT_NODE);
  }

  get textContent() {
    return this._nodes.map((node) => node.textContent).join('');
  }

  set textContent(value) {
    for (const node of this._nodes) {
      node.parentElement = null;
      node.parentNode = null;
    }
    this._nodes = [];
    this.appendChild(new FakeText(value));
    // escapeHtml() round-trips text through textContent -> innerHTML.
    this.innerHTML = escapeForFakeDom(value);
  }

  appendChild(child) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentElement = this;
    child.parentNode = this;
    this._nodes.push(child);
    return child;
  }

  removeChild(child) {
    const index = this._nodes.indexOf(child);
    if (index === -1) {
      throw new Error('child not found');
    }
    this._nodes.splice(index, 1);
    child.parentElement = null;
    child.parentNode = null;
    return child;
  }

  insertBefore(child, referenceChild) {
    if (child.parentNode) child.parentNode.removeChild(child);
    child.parentElement = this;
    child.parentNode = this;
    if (!referenceChild) {
      this._nodes.push(child);
      return child;
    }
    const index = this._nodes.indexOf(referenceChild);
    if (index === -1) {
      throw new Error('reference child not found');
    }
    this._nodes.splice(index, 0, child);
    return child;
  }

  prepend(child) {
    return this.insertBefore(child, this._nodes[0] || null);
  }

  remove() {
    if (this.parentNode) this.parentNode.removeChild(this);
  }

  get firstElementChild() {
    return this.children[0] || null;
  }

  get lastElementChild() {
    const elements = this.children;
    return elements[elements.length - 1] || null;
  }

  get firstChild() {
    return this._nodes[0] || null;
  }

  // Deterministic layout stand-in for the turn engine: inline pixel heights
  // win (spacers, placeholders), `.hidden` collapses, otherwise the height is
  // the element's own base plus the sum of its children, with a default leaf
  // height for text-bearing leaves.
  getBoundingClientRect() {
    return {height: fakeElementHeight(this)};
  }

  replaceWith(newNode) {
    const parent = this.parentNode;
    if (!parent) return;
    const index = parent._nodes.indexOf(this);
    if (index === -1) throw new Error('replaceWith: node not in parent');
    parent.removeChild(this);
    parent.insertBefore(newNode, parent._nodes[index] || null);
  }

  addEventListener(type, fn) {
    if (!this._listeners[type]) this._listeners[type] = [];
    this._listeners[type].push(fn);
  }

  removeEventListener(type, fn) {
    const list = this._listeners[type];
    if (!list) return;
    const index = list.indexOf(fn);
    if (index >= 0) list.splice(index, 1);
  }

  fire(type, event) {
    (this._listeners[type] || []).slice().forEach((fn) => fn(event || {}));
  }

  get nextSibling() {
    if (!this.parentNode) return null;
    const siblings = this.parentNode._nodes;
    const index = siblings.indexOf(this);
    return index === -1 ? null : (siblings[index + 1] || null);
  }

  get nextElementSibling() {
    if (!this.parentElement) return null;
    const siblings = this.parentElement.children;
    const index = siblings.indexOf(this);
    return index === -1 ? null : (siblings[index + 1] || null);
  }

  get previousElementSibling() {
    if (!this.parentElement) return null;
    const siblings = this.parentElement.children;
    const index = siblings.indexOf(this);
    return index <= 0 ? null : siblings[index - 1];
  }

  querySelector(selector) {
    if (!selector.startsWith('.')) {
      throw new Error(`Unsupported selector: ${selector}`);
    }
    const className = selector.slice(1);
    for (const child of this.children) {
      if (child.classList.contains(className)) {
        return child;
      }
      const nested = child.querySelector(selector);
      if (nested) return nested;
    }
    return null;
  }

  querySelectorAll(selector) {
    if (!selector.startsWith('.')) {
      throw new Error(`Unsupported selector: ${selector}`);
    }
    const className = selector.slice(1);
    const matches = [];
    for (const child of this.children) {
      if (child.classList.contains(className)) matches.push(child);
      matches.push(...child.querySelectorAll(selector));
    }
    return matches;
  }

  closest(selector) {
    if (!selector.startsWith('.')) {
      throw new Error(`Unsupported selector: ${selector}`);
    }
    const className = selector.slice(1);
    let current = this;
    while (current) {
      if (current.classList.contains(className)) {
        return current;
      }
      current = current.parentElement;
    }
    return null;
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.get(name) || null;
  }
}

function createSession(id, timeIso, timeText) {
  const session = new FakeElement('A', {id: `session-${id}`});
  const time = new FakeElement('SPAN', {className: 'session-time'});
  time.dataset.time = timeIso;
  time.textContent = timeText;
  session.appendChild(time);
  return session;
}

function loadChatContext(document) {
  const nowIso = '2026-04-02T03:04:05.000Z';
  const context = {
    SESSION_ID: 'session-a',
    document: {
      addEventListener() {},
      createElement(tag) {
        return new FakeElement(tag);
      },
      ...document,
    },
    console: {error: () => {}},
    relativeTime: (iso) => `relative:${iso}`,
    window: {addEventListener() {}},
    // Real parsing (the fold row formats a message timestamp), fixed "now"
    // (the sidebar bump stamps the current time).
    Date: class FakeDate extends Date {
      toISOString() {
        return nowIso;
      }
    },
  };

  vm.createContext(context);
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  return {context, nowIso};
}

test('bumpCurrentSessionToTop keeps grouped sessions inside their current group', () => {
  const nav = new FakeElement('DIV', {id: 'session-list'});
  const group = new FakeElement('DIV', {className: 'session-group'});
  const toggle = new FakeElement('DIV');
  const items = new FakeElement('DIV', {className: 'session-group-items'});
  const before = createSession('session-b', '2026-04-01T00:00:00.000Z', 'old-b');
  const current = createSession('session-a', '2026-04-01T01:00:00.000Z', 'old-a');
  const after = createSession('session-c', '2026-04-01T02:00:00.000Z', 'old-c');

  items.appendChild(before);
  items.appendChild(current);
  items.appendChild(after);
  group.appendChild(toggle);
  group.appendChild(items);
  nav.appendChild(group);

  const {context, nowIso} = loadChatContext({
    getElementById(id) {
      if (id === 'session-list') return nav;
      if (id === 'session-session-a') return current;
      return null;
    },
  });

  context.bumpCurrentSessionToTop();

  assert.equal(current.parentElement, items);
  assert.deepEqual(items.children.map((child) => child.id), [
    'session-session-a',
    'session-session-b',
    'session-session-c',
  ]);
  assert.equal(nav.firstElementChild, group);
  assert.equal(current.querySelector('.session-time').dataset.time, nowIso);
  assert.equal(current.querySelector('.session-time').textContent, `relative:${nowIso}`);
});

test('bumpCurrentSessionToTop moves flat sidebar sessions to the top-level front', () => {
  const nav = new FakeElement('DIV', {id: 'session-list'});
  const before = createSession('session-b', '2026-04-01T00:00:00.000Z', 'old-b');
  const current = createSession('session-a', '2026-04-01T01:00:00.000Z', 'old-a');
  const after = createSession('session-c', '2026-04-01T02:00:00.000Z', 'old-c');

  nav.appendChild(before);
  nav.appendChild(current);
  nav.appendChild(after);

  const {context, nowIso} = loadChatContext({
    getElementById(id) {
      if (id === 'session-list') return nav;
      if (id === 'session-session-a') return current;
      return null;
    },
  });

  context.bumpCurrentSessionToTop();

  assert.equal(current.parentElement, nav);
  assert.deepEqual(nav.children.map((child) => child.id), [
    'session-session-a',
    'session-session-b',
    'session-session-c',
  ]);
  assert.equal(current.querySelector('.session-time').dataset.time, nowIso);
  assert.equal(current.querySelector('.session-time').textContent, `relative:${nowIso}`);
});

// ---------------------------------------------------------------------------
// Turn outline fold — invariants I0-I6.
//
// Every expected value below is computed from the test's own input spec, never
// from the implementation: `expectedTurns()` re-derives the turn boundaries
// from the item list, and the row fields come from the item that the generator
// placed at the head / conclusion. The only thing read out of the DOM is what
// the derive produced.
// ---------------------------------------------------------------------------
const STIMULUS_ROLES = ['user', 'scheduled_trigger', 'worker_summary'];
const PROSE_ROLES = ['assistant', 'worker_summary', 'plan'];
const TYPE_LABELS = {user: 'You', scheduled_trigger: 'Trigger', worker_summary: 'Worker'};
const DEPTHS = ['outline', 'compact', 'expanded'];
const BUBBLE_TIME_TEXT = 'Apr 2, 2026, 3:04:05 AM PDT';

// --- input spec -> DOM -----------------------------------------------------
function msg(role, id, extra = {}) {
  return Object.assign({kind: 'msg', role, id, text: `${role} ${id}`, ts: null}, extra);
}

function separator(id, extra = {}) {
  return Object.assign({kind: 'msg', role: 'separator', id, secs: 42}, extra);
}

function plain(id) {
  return {kind: 'plain', id};
}

// #streaming-msg / #load-more-sentinel: container fixtures inside no span.
function fixture(id) {
  return {kind: 'fixture', id};
}

function appendTimeDiv(el, ts) {
  if (!ts) return;
  const time = new FakeElement('DIV', {className: 'text-[10px] mt-1'});
  time.textContent = BUBBLE_TIME_TEXT;
  el.appendChild(time);
}

// Mirrors where each renderMessage() branch puts its text: prose bubbles keep
// the unrendered markdown on `.prose-msg[data-raw]`, plain bubbles hold it as
// text next to their time div.
function buildElement(item) {
  const el = new FakeElement('DIV');
  el.dataset.nodeId = item.id;
  if (item.kind !== 'msg') {
    el.id = item.kind === 'fixture' ? item.id : '';
    el.textContent = `node ${item.id}`;
    return el;
  }
  el.dataset.messageId = item.id;
  el.dataset.messageRole = item.role;
  if (item.ts) el.dataset.messageTs = item.ts;

  if (item.role === 'separator') {
    el.className = 'separator-line group/sep';
    if (item.secs != null) el.dataset.thinkingSeconds = String(item.secs);
    const line = new FakeElement('DIV', {className: 'flex-1 border-t'});
    el.appendChild(line);
    return el;
  }

  // The trigger bubble is the one that holds its text and its time div under
  // the same `.whitespace-pre-wrap` node, as renderMessage() writes it.
  const bubbleClass = {
    user: 'max-w-[75%]',
    scheduled_trigger: 'w-full whitespace-pre-wrap break-words',
  }[item.role] || 'max-w-[90%]';
  const bubble = new FakeElement('DIV', {className: bubbleClass});
  if (PROSE_ROLES.includes(item.role)) {
    const prose = new FakeElement('DIV', {className: 'prose-msg'});
    prose.dataset.raw = item.text;
    bubble.appendChild(prose);
  } else if (item.role === 'user') {
    if (item.voice) {
      const badge = new FakeElement('SPAN', {className: 'text-xs text-blue-200 block mb-1'});
      badge.textContent = '\u{1F3A4} Voice';
      bubble.appendChild(badge);
    }
    const text = new FakeElement('DIV', {className: 'whitespace-pre-wrap'});
    text.textContent = item.text;
    bubble.appendChild(text);
  } else {
    bubble.appendChild(new FakeText(item.text));
  }
  appendTimeDiv(bubble, item.ts);
  el.appendChild(bubble);
  return el;
}

function mountCase(items) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  const control = new FakeElement('DIV', {id: 'page-depth-control'});
  for (const depth of DEPTHS) {
    const btn = new FakeElement('BUTTON', {className: 'turn-depth-btn'});
    btn.dataset.pageDepth = depth;
    control.appendChild(btn);
  }
  const nodes = new Map();
  for (const item of items) {
    const el = buildElement(item);
    nodes.set(item, el);
    root.appendChild(el);
  }
  const {context} = loadChatContext({
    getElementById(id) {
      if (id === 'messages') return root;
      if (id === 'page-depth-control') return control;
      return null;
    },
  });
  return {context, root, control, nodes};
}

// --- independent expectations ----------------------------------------------
function expectedFirstLine(text) {
  for (const rawLine of String(text).split('\n')) {
    let line = rawLine.trim();
    line = line.replace(/^#{1,6}\s+/, '');
    line = line.replace(/^[-*+>]\s+/, '');
    line = line.replace(/^\d+[.)]\s+/, '');
    line = line.replace(/\[([^\]]*)\]\([^)]*\)/g, '$1');
    line = line.split('`').join('').trim();
    if (line) return line;
  }
  return '';
}

function expectedSteps(count) {
  if (!count) return '';
  return `${count} step${count === 1 ? '' : 's'}`;
}

function expectedDuration(secs) {
  if (secs == null) return '';
  if (secs < 60) return `${secs}s`;
  return `${Math.floor(secs / 60)}m${String(secs % 60).padStart(2, '0')}s`;
}

function expectedTime(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

function lastMatch(items, predicate) {
  let found = null;
  for (const item of items) if (predicate(item)) found = item;
  return found;
}

// The span cut and the head/conclusion/fold-range picks, recomputed from the
// item list alone.
function expectedTurns(items) {
  const spans = [];
  let span = [];
  for (const item of items) {
    if (item.kind === 'fixture') continue;
    span.push(item);
    if (item.kind === 'msg' && item.role === 'separator') {
      spans.push({items: span});
      span = [];
    }
  }
  return spans.map(describeExpectedSpan).filter(Boolean);
}

function describeExpectedSpan(span) {
  const messages = span.items.filter((item) => item.kind === 'msg');
  const tail = messages[messages.length - 1];
  const body = messages.slice(0, -1);
  const conclusion = lastMatch(body, (item) => item.role === 'assistant');
  const beforeConclusion = conclusion ? body.slice(0, body.indexOf(conclusion)) : body;
  const stimulus = lastMatch(beforeConclusion, (item) => item.role === 'user')
    || lastMatch(beforeConclusion, (item) => STIMULUS_ROLES.includes(item.role));
  const head = stimulus || body[0];
  if (!head) return null;
  const steps = conclusion
    ? body.slice(body.indexOf(head) + 1, body.indexOf(conclusion))
    : [];
  return {items: span.items, head, conclusion, tail, steps};
}

function expectedRow(turn) {
  const head = turn.head;
  const title = head.role === 'worker_summary' && head.workerId
    ? head.workerId
    : expectedFirstLine(head.text);
  return {
    tag: TYPE_LABELS[head.role] || 'Turn',
    title,
    conclusion: turn.conclusion ? expectedFirstLine(turn.conclusion.text) : '',
    steps: expectedSteps(turn.steps.length),
    duration: expectedDuration(turn.tail.secs),
    time: expectedTime(head.ts),
  };
}

// --- DOM readers ------------------------------------------------------------
function wrappers(root) {
  return root.children.filter((el) => el.classList.contains('turn-wrap'));
}

function nodeLabel(node) {
  return node.dataset.nodeId || node.className || node.tagName;
}

function descendantNodes(node, allowed) {
  const found = [];
  for (const child of node.children) {
    if (allowed.has(child)) found.push(child);
    found.push(...descendantNodes(child, allowed));
  }
  return found;
}

function stableMessageIds(root) {
  const ids = [];
  const walk = (node) => {
    for (const child of node.children) {
      if (child.dataset.messageId && child.dataset.messageRole) ids.push(child.dataset.messageId);
      walk(child);
    }
  };
  walk(root);
  return ids.sort();
}

function snapshot(node) {
  if (node.nodeType === TEXT_NODE) return {text: node.textContent};
  return {
    tag: node.tagName,
    className: node.className,
    dataset: Object.assign({}, node.dataset),
    attributes: Object.fromEntries(node.attributes),
    children: node.childNodes.map(snapshot),
  };
}

function rowField(row, className) {
  const field = row.querySelector(`.${className}`);
  assert.ok(field, `row is missing .${className}`);
  return field.textContent;
}

// --- invariants -------------------------------------------------------------
// I0 — wrapper completeness. `nodes` maps every input item to the element the
// test created for it, so filtering a wrapper's descendants by that set drops
// everything the derive added (row, fold bar, fold content) and leaves exactly
// the span the wrapper claims to hold.
function assertWrapperCompleteness(root, turns, nodes, label) {
  const found = wrappers(root);
  assert.equal(found.length, turns.length, `${label}: wrapper count`);
  found.forEach((wrap, i) => {
    const held = descendantNodes(wrap, new Set(nodes.values()));
    const wanted = turns[i].items.map((item) => nodes.get(item));
    assert.deepEqual(held.map(nodeLabel), wanted.map(nodeLabel), `${label}: wrapper ${i} span`);
    held.forEach((node, j) => {
      assert.ok(node === wanted[j], `${label}: wrapper ${i} node ${j} is not the input node`);
    });
    assert.equal(wrap.querySelectorAll('.turn-row').length, 1, `${label}: wrapper ${i} rows`);
  });
}

// I2 — open/folded state equals open(turn), and the row's six fields equal the
// independently computed values. Row values do not depend on the state, so
// they are checked on every wrapper, not only the folded ones.
function assertStateAndRows(root, turns, depth, label) {
  const found = wrappers(root);
  found.forEach((wrap, i) => {
    const override = wrap.dataset.turnOverride;
    const open = override
      ? override === 'open'
      : (depth === 'outline' ? i === found.length - 1 : true);
    assert.equal(wrap.dataset.turnOpen, String(open), `${label}: wrapper ${i} open state`);

    const row = wrap.querySelector('.turn-row');
    const want = expectedRow(turns[i]);
    assert.equal(rowField(row, 'turn-row-tag'), want.tag, `${label}: wrapper ${i} tag`);
    assert.equal(rowField(row, 'turn-row-title'), want.title, `${label}: wrapper ${i} title`);
    assert.equal(rowField(row, 'turn-row-conclusion'), want.conclusion,
        `${label}: wrapper ${i} conclusion line`);
    assert.equal(rowField(row, 'turn-row-steps'), want.steps, `${label}: wrapper ${i} steps`);
    assert.equal(rowField(row, 'turn-row-duration'), want.duration, `${label}: wrapper ${i} duration`);
    assert.equal(rowField(row, 'turn-row-time'), want.time, `${label}: wrapper ${i} time`);
  });
}

// I4 — the stimulus stays out of the `N steps` band, so opening a turn always
// shows what triggered it.
function assertStimulusVisible(root, turns, nodes, label) {
  wrappers(root).forEach((wrap, i) => {
    const head = nodes.get(turns[i].head);
    assert.equal(head.closest('.turn-fold-content'), null,
        `${label}: wrapper ${i} head is inside the fold band`);
    assert.equal(head.parentElement, wrap, `${label}: wrapper ${i} head is not a direct child`);
  });
}

// I6 — everything outside a finished turn stays flat at container level.
function assertFlatBoundaries(root, turns, nodes, label) {
  const wrapped = new Set(turns.flatMap((turn) => turn.items).map((item) => nodes.get(item)));
  for (const node of nodes.values()) {
    if (wrapped.has(node)) continue;
    assert.equal(node.closest('.turn-wrap'), null, `${label}: ${nodeLabel(node)} was wrapped`);
    assert.equal(node.parentElement, root, `${label}: ${nodeLabel(node)} left container level`);
  }
}

// --- generator --------------------------------------------------------------
const HEAD_ROLES = [
  'user',
  'scheduled_trigger',
  'worker_summary',
  'assistant',
  'system',
  'task_delegated',
  'clone_start',
];

const HEAD_TEXTS = [
  'fold the messages page',
  '## cuda_muon status\n\nthe run finished',
  '\n\n- check the `separator` markup\nsecond line',
  'read [the plan](/artifacts/plan_02.html) first',
];

const CONCLUSION_TEXTS = [
  'both pushes landed, the PR is refreshed',
  '### Verdict\nall three criteria pass',
  '\n1. rebased onto `main`\n2. merged',
];

function headItem(role, id, index) {
  if (role === 'worker_summary') {
    const workerId = `a1b2c3d${index % 10}`;
    return msg(role, id, {
      text: `Worker \`${workerId}\` | thread \`${workerId}-4f21-9c3e\` | status: completed`,
      workerId,
      ts: '2026-04-02T10:07:00.000Z',
    });
  }
  return msg(role, id, {text: HEAD_TEXTS[index % HEAD_TEXTS.length], ts: '2026-04-02T10:07:00.000Z'});
}

// A finished turn: head, `steps` intermediate messages, a conclusion and a
// separator. `stepRoles` lets a non-message node or an extra card sit in the
// span without becoming part of the fold band.
function finishedTurn(prefix, {headRole = 'user', steps = 1, index = 0, secs = 42, ts, pre = []} = {}) {
  const head = headItem(headRole, `${prefix}-head`, index);
  if (ts !== undefined) head.ts = ts;
  const items = pre.map((role, i) => msg(role, `${prefix}-pre${i}`));
  items.push(head);
  for (let i = 0; i < steps; i++) {
    items.push(msg(i % 2 === 0 ? 'assistant' : 'system', `${prefix}-step${i}`));
  }
  items.push(msg('assistant', `${prefix}-concl`, {
    text: CONCLUSION_TEXTS[index % CONCLUSION_TEXTS.length],
  }));
  items.push(separator(`${prefix}-sep`, {secs}));
  return items;
}

function liveTail(prefix) {
  return [msg('user', `${prefix}-live-head`), msg('assistant', `${prefix}-live-body`)];
}

function generateCases() {
  const cases = [];
  let index = 0;
  for (const headRole of HEAD_ROLES) {
    for (const steps of [0, 1, 3]) {
      for (const terminated of [true, false]) {
        index++;
        const items = [
          ...finishedTurn('t1', {index}),
          ...finishedTurn('t2', {headRole, steps, index}),
          ...(terminated ? finishedTurn('t3', {index: index + 1, steps: 2}) : liveTail('t3')),
        ];
        cases.push({name: `${headRole}/${steps} steps/${terminated ? 'terminated' : 'live'}`, items});
      }
    }
  }

  cases.push({
    // The 0.65% shape: a Delegated card lands in the head position and the real
    // stimulus arrives after it.
    name: 'delegated card occupies the head position',
    items: [
      ...finishedTurn('a', {}),
      ...finishedTurn('b', {pre: ['task_delegated'], steps: 2}),
      ...liveTail('c'),
    ],
  });
  cases.push({
    // Two stimuli in one span (a worker summary lands while the master is
    // answering the user's ask): the user prompt wins the head over the later
    // worker_summary, so the tag is "You" and the title is the user text.
    name: 'user prompt wins over worker_summary in one span',
    items: [
      ...finishedTurn('h1', {}),
      msg('user', 'h2-first', {text: 'first ask', ts: '2026-04-02T13:00:00.000Z'}),
      msg('worker_summary', 'h2-second', {
        text: 'Worker `beef1234` | thread `beef1234-11aa` | status: completed',
        workerId: 'beef1234',
        ts: '2026-04-02T13:04:00.000Z',
      }),
      msg('system', 'h2-step'),
      msg('assistant', 'h2-concl', {text: 'answered both'}),
      separator('h2-sep', {secs: 91}),
    ],
  });
  cases.push({
    // A stimulus that arrives after the conclusion belongs to the next round,
    // so it must not become this turn's head.
    name: 'stimulus after the conclusion',
    items: [
      ...finishedTurn('i1', {}),
      msg('user', 'i2-head', {text: 'the ask being answered', ts: '2026-04-02T14:00:00.000Z'}),
      msg('assistant', 'i2-step'),
      msg('assistant', 'i2-concl', {text: 'here is the answer'}),
      msg('user', 'i2-late', {text: 'one more thing', ts: '2026-04-02T14:09:00.000Z'}),
      separator('i2-sep', {secs: 33}),
    ],
  });
  cases.push({
    // The auto-wake shape: a stimulus-free but complete turn sits at the
    // container top; it folds with the body[0] head fallback.
    name: 'stimulus-free leading turn at the container top',
    items: [
      msg('assistant', 'p-a1'),
      msg('system', 'p-s1'),
      msg('assistant', 'p-a2'),
      separator('p-sep'),
      ...finishedTurn('q', {}),
    ],
  });
  cases.push({
    name: 'load-more sentinel above the first span',
    items: [fixture('load-more-sentinel'), ...finishedTurn('s', {}), ...finishedTurn('s2', {steps: 2})],
  });
  cases.push({
    name: 'streaming node below the last span',
    items: [...finishedTurn('m', {}), ...finishedTurn('m2', {}), fixture('streaming-msg')],
  });
  cases.push({
    name: 'non-message nodes inside the span',
    items: [
      ...finishedTurn('n', {}),
      plain('n2-card'),
      msg('user', 'n2-head', {text: 'a question', ts: '2026-04-02T11:30:00.000Z'}),
      plain('n2-note'),
      msg('assistant', 'n2-step'),
      msg('assistant', 'n2-concl', {text: 'the answer'}),
      separator('n2-sep', {secs: 605}),
    ],
  });
  cases.push({
    name: 'turn without a conclusion',
    items: [
      ...finishedTurn('c1', {}),
      msg('user', 'c2-head', {text: 'anything?', ts: '2026-04-02T12:00:00.000Z'}),
      msg('system', 'c2-error', {text: 'Error: backend refused'}),
      separator('c2-sep', {secs: 7}),
    ],
  });
  cases.push({
    name: 'bare separator span',
    items: [...finishedTurn('d1', {}), separator('d-bare'), ...finishedTurn('d2', {})],
  });
  cases.push({
    name: 'head without a timestamp and separator without a duration',
    items: [
      ...finishedTurn('e1', {}),
      ...finishedTurn('e2', {ts: null, secs: null}),
    ],
  });
  cases.push({
    name: 'worker summary without a worker id',
    items: [
      ...finishedTurn('w1', {}),
      msg('worker_summary', 'w2-head', {text: 'worker finished, no locator', ts: '2026-04-02T09:00:00.000Z'}),
      msg('assistant', 'w2-step'),
      msg('assistant', 'w2-concl', {text: 'merged back'}),
      separator('w2-sep', {secs: 3600}),
    ],
  });
  cases.push({
    // The voice badge and the bubble's time div both sit next to the text and
    // must not become the title.
    name: 'voice user head',
    items: [
      ...finishedTurn('v1', {}),
      msg('user', 'v2-head', {text: 'read the fold plan', voice: true, ts: '2026-04-02T15:20:00.000Z'}),
      msg('assistant', 'v2-step'),
      msg('assistant', 'v2-concl', {text: 'read it, here is the summary'}),
      separator('v2-sep', {secs: 54}),
    ],
  });
  cases.push({
    name: 'single finished turn',
    items: finishedTurn('only', {steps: 2}),
  });
  return cases;
}

// --- I0/I2/I4/I6 over every generated case, at every depth ------------------
for (const depth of DEPTHS) {
  test(`turn outline: wrapper completeness, state, row values and flat boundaries at depth ${depth}`, () => {
    for (const testCase of generateCases()) {
      const label = `${testCase.name} @ ${depth}`;
      const {context, root, nodes} = mountCase(testCase.items);
      const turns = expectedTurns(testCase.items);

      context.setPageDepth(depth);

      assertWrapperCompleteness(root, turns, nodes, label);
      assertStateAndRows(root, turns, depth, label);
      assertStimulusVisible(root, turns, nodes, label);
      assertFlatBoundaries(root, turns, nodes, label);
    }
  });
}

// --- I1 + I3 ---------------------------------------------------------------
test('turn outline: message conservation and idempotence across repeated derives at every depth', () => {
  for (const testCase of generateCases()) {
    const {context, root} = mountCase(testCase.items);
    const flatIds = stableMessageIds(root);
    assert.ok(flatIds.length > 0, `${testCase.name}: fixture has no stable messages`);

    for (const depth of DEPTHS) {
      context.setPageDepth(depth);
      assert.deepEqual(stableMessageIds(root), flatIds, `${testCase.name} @ ${depth}: conservation`);

      const before = snapshot(root);
      context.applyTurnOutline(root);
      assert.deepEqual(snapshot(root), before, `${testCase.name} @ ${depth}: idempotence`);
      context.applyTurnOutline(root);
      assert.deepEqual(snapshot(root), before, `${testCase.name} @ ${depth}: idempotence (third derive)`);
      assert.deepEqual(stableMessageIds(root), flatIds,
          `${testCase.name} @ ${depth}: conservation after repeats`);
    }
  }
});

// --- I0 is not vacuous ------------------------------------------------------
test('I0 fails for a no-op derive and for a derive that only wraps the N steps band', () => {
  const items = [...finishedTurn('x1', {steps: 2}), ...finishedTurn('x2', {steps: 2})];
  const turns = expectedTurns(items);

  const noop = mountCase(items);
  assert.throws(
      () => assertWrapperCompleteness(noop.root, turns, noop.nodes, 'no-op'),
      /wrapper count/);

  // Today's shape: the intermediate messages get folded into a band, but no
  // turn is wrapped.
  const banded = mountCase(items);
  for (const turn of turns) {
    const band = new FakeElement('DIV', {className: 'turn-fold-content'});
    const first = banded.nodes.get(turn.steps[0]);
    banded.root.insertBefore(band, first);
    for (const step of turn.steps) band.appendChild(banded.nodes.get(step));
  }
  assert.throws(
      () => assertWrapperCompleteness(banded.root, turns, banded.nodes, 'band only'),
      /wrapper count/);
});

// --- I2: fold on completion -------------------------------------------------
test('I2: a landing separator wraps the finished turn and folds the one before it', () => {
  const items = [...finishedTurn('k1', {}), ...finishedTurn('k2', {steps: 2}), ...liveTail('k3')];
  const {context, root, nodes} = mountCase(items);
  context.setPageDepth('outline');

  let turns = expectedTurns(items);
  assert.equal(wrappers(root).length, 2);
  assertStateAndRows(root, turns, 'outline', 'before landing');
  assert.equal(wrappers(root)[0].dataset.turnOpen, 'false');
  assert.equal(wrappers(root)[1].dataset.turnOpen, 'true');

  // The live turn finishes: its separator lands at the end of the container.
  const landed = separator('k3-sep', {secs: 68});
  items.push(landed);
  const landedEl = buildElement(landed);
  nodes.set(landed, landedEl);
  root.appendChild(landedEl);
  context.applyTurnOutline(root);

  turns = expectedTurns(items);
  assert.equal(wrappers(root).length, 3);
  assertWrapperCompleteness(root, turns, nodes, 'after landing');
  assertStateAndRows(root, turns, 'outline', 'after landing');
  assert.deepEqual(wrappers(root).map((wrap) => wrap.dataset.turnOpen), ['false', 'false', 'true']);
});

// --- I5 ---------------------------------------------------------------------
test('I5: manual open/fold, an expanded N steps bar and an open recap panel survive later derives', () => {
  const items = [
    ...finishedTurn('r1', {steps: 2}),
    ...finishedTurn('r2', {steps: 3}),
    ...finishedTurn('r3', {steps: 2}),
  ];
  const {context, root, nodes} = mountCase(items);
  context.setPageDepth('outline');

  const [first, second, third] = wrappers(root);

  // Manual state: open the first turn from its row, fold the last one from the
  // collapse control on its separator line.
  const firstRow = first.querySelector('.turn-row');
  firstRow.onclick.call(firstRow);
  const collapse = third.querySelector('.turn-collapse');
  collapse.onclick.call(collapse);
  assert.equal(first.dataset.turnOpen, 'true');
  assert.equal(third.dataset.turnOpen, 'false');

  // Reader-expanded `N steps` bar in the second turn.
  const bar = second.querySelector('.turn-fold-bar');
  const band = second.querySelector('.turn-fold-content');
  context.toggleTurnFold(bar);
  assert.equal(band.classList.contains('hidden'), false);
  assert.equal(bar.getAttribute('aria-expanded'), 'true');

  // Recap panel pulled open next to the second turn's separator, exactly where
  // toggleRecapPanel() puts it.
  const panel = new FakeElement('DIV', {className: 'recap-panel'});
  panel.textContent = 'What was discussed';
  const sep = nodes.get(items.find((item) => item.id === 'r2-sep'));
  sep.parentNode.insertBefore(panel, sep.nextElementSibling);
  const panelIndex = second.childNodes.indexOf(panel);

  const bandContents = band.children.slice();
  context.applyTurnOutline(root);
  context.applyTurnOutline(root);

  assert.equal(first.dataset.turnOpen, 'true', 'manual open survives');
  assert.equal(third.dataset.turnOpen, 'false', 'manual fold survives');
  assert.equal(band.classList.contains('hidden'), false, 'expanded bar survives');
  assert.equal(bar.getAttribute('aria-expanded'), 'true');
  assert.deepEqual(band.children, bandContents, 'band contents unmoved');
  assert.equal(panel.parentElement, second, 'recap panel stays in its wrapper');
  assert.equal(second.childNodes.indexOf(panel), panelIndex, 'recap panel stays in place');
  assert.equal(panel.textContent, 'What was discussed', 'recap panel content untouched');

  // A newly finished turn arriving later must not disturb any of it.
  const landing = finishedTurn('r4', {steps: 1});
  for (const item of landing) {
    const el = buildElement(item);
    nodes.set(item, el);
    root.appendChild(el);
    items.push(item);
  }
  context.applyTurnOutline(root);

  assert.equal(first.dataset.turnOpen, 'true');
  assert.equal(third.dataset.turnOpen, 'false');
  assert.equal(band.classList.contains('hidden'), false);
  assert.equal(panel.parentElement, second);
  assert.equal(second.childNodes.indexOf(panel), panelIndex);
  assert.equal(wrappers(root).length, 4);
  assert.equal(wrappers(root)[3].dataset.turnOpen, 'true');
});

// --- I6: pagination --------------------------------------------------------
// Turn-aligned paging means a page always begins at a turn start, so a page
// top can only ever hold complete turns. Any separator-terminated span at the
// container top wraps — whether or not it carries a stimulus.
test('I6: a complete separator-terminated span at the container top wraps, with or without a stimulus', () => {
  // Without a stimulus: the head falls back to the body's first message.
  const bareItems = [
    msg('assistant', 'a1'),
    msg('system', 's1'),
    msg('assistant', 'a2'),
    separator('sep1', {secs: 12}),
  ];
  const bareCase = mountCase(bareItems);
  bareCase.context.setPageDepth('outline');

  const bareTurns = expectedTurns(bareItems);
  assert.equal(bareTurns.length, 1);
  assert.equal(bareTurns[0].head, bareItems[0]);
  assertWrapperCompleteness(bareCase.root, bareTurns, bareCase.nodes, 'stimulus-free top turn');
  assertStateAndRows(bareCase.root, bareTurns, 'outline', 'stimulus-free top turn');

  // With a stimulus: the same span wraps with the stimulus as head.
  const items = [
    msg('user', 'u1', {text: 'the original ask', ts: '2026-04-02T08:15:00.000Z'}),
    msg('assistant', 'a1'),
    msg('system', 's1'),
    msg('assistant', 'a2'),
    separator('sep1', {secs: 12}),
  ];
  const {context, root, nodes} = mountCase(items);
  context.setPageDepth('outline');

  const turns = expectedTurns(items);
  assert.equal(turns.length, 1);
  assertWrapperCompleteness(root, turns, nodes, 'stimulated top turn');
  assertStateAndRows(root, turns, 'outline', 'stimulated top turn');
  assert.equal(root.querySelectorAll('.turn-fold-bar').length, 1);
  assert.deepEqual(
      root.querySelector('.turn-fold-content').children.map((child) => child.dataset.messageId),
      ['a1', 's1']);
});

// The skip_user_event auto-wake shape: a complete turn whose body is only
// assistant messages. At the container top it still folds, and its row's head
// is that first assistant message.
test('a stimulus-free complete turn at the container top folds with its first assistant as head', () => {
  const items = [
    msg('assistant', 'w1', {text: 'compiling the round plan'}),
    msg('assistant', 'w2', {text: 'running the checks'}),
    msg('assistant', 'w3', {text: 'all green'}),
    separator('wake-sep', {secs: 31}),
  ];
  const {context, root, nodes} = mountCase(items);
  context.setPageDepth('outline');

  const turns = expectedTurns(items);
  assert.equal(turns.length, 1);
  assert.equal(turns[0].head, items[0], 'head must be the first assistant message');
  assert.equal(turns[0].head.role, 'assistant');
  assertWrapperCompleteness(root, turns, nodes, 'wake turn');
  assertStateAndRows(root, turns, 'outline', 'wake turn');
  assertStimulusVisible(root, turns, nodes, 'wake turn');
});

// The orphan defect under turn-aligned input: a first paint starting at a
// turn start, then an older page ending in a separator prepended above it.
// No message may be left outside a turn wrapper.
test('load-more under turn-aligned paging leaves no message outside a turn wrapper', () => {
  const pageItems = [
    msg('user', 'p1-head', {text: 'the ask', ts: '2026-04-02T09:00:00.000Z'}),
    msg('assistant', 'p1-step'),
    msg('assistant', 'p1-concl', {text: 'the answer'}),
    separator('p1-sep', {secs: 55}),
  ];
  const {context, root, nodes} = mountCase(pageItems);
  context.setPageDepth('outline');
  assert.equal(wrappers(root).length, 1, 'first paint wraps the complete turn');

  const olderItems = [
    msg('user', 'p0-head', {text: 'an earlier ask', ts: '2026-04-02T08:00:00.000Z'}),
    msg('assistant', 'p0-concl', {text: 'an earlier answer'}),
    separator('p0-sep', {secs: 40}),
  ];
  const olderEls = olderItems.map((item) => {
    const el = buildElement(item);
    nodes.set(item, el);
    return el;
  });
  for (let i = olderEls.length - 1; i >= 0; i--) root.prepend(olderEls[i]);
  context.applyTurnOutline(root);
  context.applyTurnOutline(root);  // repeated derives must not disturb it

  const turns = expectedTurns([...olderItems, ...pageItems]);
  assertWrapperCompleteness(root, turns, nodes, 'after load-more');
  assertStateAndRows(root, turns, 'outline', 'after load-more');
  for (const el of nodes.values()) {
    assert.ok(el.closest('.turn-wrap'), `${el.dataset.messageId} is outside every turn wrapper`);
  }
});

// --- the row's two value sources -------------------------------------------
test('renderMessage emits data-message-ts and data-thinking-seconds, and omits both when absent', () => {
  const {context} = loadChatContext({
    getElementById() {
      return null;
    },
  });
  const ts = '2026-04-02T10:07:00.000Z';

  const separatorHtml = context.renderMessage(
      {role: 'separator', id: 'sep1', thinking_seconds: 68, event_index: 3, timestamp: ts}, 'sess-1');
  assert.ok(separatorHtml.includes('data-thinking-seconds="68"'), separatorHtml);
  assert.ok(separatorHtml.includes(`data-message-ts="${ts}"`), separatorHtml);

  const userHtml = context.renderMessage({role: 'user', id: 'u1', content: 'hi', timestamp: ts}, 'sess-1');
  assert.ok(userHtml.includes(`data-message-ts="${ts}"`), userHtml);

  // No timestamp and no thinking time: no attribute at all, so the row's own
  // fields stay empty rather than showing a placeholder.
  const bare = context.renderMessage({role: 'separator', id: 'sep2', event_index: 4}, 'sess-1');
  assert.ok(!bare.includes('data-message-ts='), bare);
  assert.ok(!bare.includes('data-thinking-seconds='), bare);
});

// --- retained `N steps` bar behaviour --------------------------------------
test('turn fold bar toggles its single intermediate span', () => {
  const items = [
    msg('user', 'u1'),
    msg('assistant', 'a1'),
    msg('assistant', 'a2'),
    separator('sep1'),
  ];
  const {context, root} = mountCase(items);
  context.applyTurnOutline(root);

  const bar = root.querySelector('.turn-fold-bar');
  const content = root.querySelector('.turn-fold-content');

  assert.equal(content.classList.contains('hidden'), true);
  assert.equal(bar.getAttribute('aria-expanded'), 'false');

  context.toggleTurnFold(bar);

  assert.equal(content.classList.contains('hidden'), false);
  assert.equal(bar.getAttribute('aria-expanded'), 'true');
});

test('the page depth control drives wrapper state, the N steps bars and its own pressed state', () => {
  const items = [...finishedTurn('g1', {steps: 2}), ...finishedTurn('g2', {steps: 2})];
  const {context, root, control} = mountCase(items);

  const pressed = () => control.children
      .filter((btn) => btn.getAttribute('aria-pressed') === 'true')
      .map((btn) => btn.dataset.pageDepth);
  const bands = () => root.querySelectorAll('.turn-fold-content');
  const openStates = () => wrappers(root).map((wrap) => wrap.dataset.turnOpen);

  context.setPageDepth('outline');
  assert.deepEqual(pressed(), ['outline']);
  assert.deepEqual(openStates(), ['false', 'true']);
  assert.equal(bands().length, 2);
  assert.equal(bands().every((band) => band.classList.contains('hidden')), true);

  context.setPageDepth('compact');
  assert.deepEqual(pressed(), ['compact']);
  assert.deepEqual(openStates(), ['true', 'true']);
  assert.equal(bands().every((band) => band.classList.contains('hidden')), true);

  context.setPageDepth('expanded');
  assert.deepEqual(pressed(), ['expanded']);
  assert.deepEqual(openStates(), ['true', 'true']);
  assert.equal(bands().every((band) => !band.classList.contains('hidden')), true);

  context.setPageDepth('outline');
  assert.deepEqual(pressed(), ['outline']);
  assert.deepEqual(openStates(), ['false', 'true']);
  assert.equal(bands().every((band) => band.classList.contains('hidden')), true);
});

// ---------------------------------------------------------------------------
// Turn window engine — plan 1 v7 invariants.
//
// DOM = top spacer + one contiguous turn window near the viewport + bottom
// spacer (+#streaming-msg fixture). Fetched messages live in the engine's
// store; HTML build/postprocess runs in scroll-gated idle slices; folded
// turn bodies and out-of-window turns are never in the DOM. These tests drive
// the engine through its public surface (mount, scroll events, pagination
// ingest, override/toggle globals) on the same fake DOM as the legacy suites,
// extended with deterministic layout (fakeElementHeight) and queueable timers
// so every scheduling step is explicit.
// ---------------------------------------------------------------------------

// --- engine fixtures --------------------------------------------------------
function eMsg(role, id, content, extra = {}) {
  return Object.assign(
      {role, id, content: content == null ? `${role} ${id}` : content, timestamp: '2026-04-02T10:07:00.000Z'},
      extra);
}

// A finished turn: user head, `steps` intermediate messages, assistant
// conclusion, separator with an event index (recap restore needs one).
function eTurn(prefix, i, steps = 2) {
  const msgs = [eMsg('user', `${prefix}h${i}`, `question ${prefix} number ${i}`)];
  for (let s = 0; s < steps; s++) {
    msgs.push(eMsg(s % 2 ? 'system' : 'assistant', `${prefix}s${i}n${s}`, `step notice ${s}`));
  }
  msgs.push(eMsg('assistant', `${prefix}c${i}`, `the answer for ${i}`));
  msgs.push(eMsg('separator', `${prefix}p${i}`, '', {thinking_seconds: 30 + i, event_index: 1000 + i}));
  return msgs;
}

function ePage(prefix, turnCount, steps = 2) {
  const msgs = [];
  for (let i = 0; i < turnCount; i++) msgs.push(...eTurn(`${prefix}t${i}_`, 0, steps));
  return msgs;
}

function eTurnKey(prefix, i) {
  return `${prefix}h${i}|${prefix}c${i}|${prefix}p${i}`;
}

// The engine's message-node factory hook: mirrors where renderMessage puts
// identity, text and the separator's recap/collapse anchors.
function fakeEngineNode(msg) {
  const el = new FakeElement('DIV');
  if (msg.id != null) el.dataset.messageId = String(msg.id);
  el.dataset.messageRole = msg.role;
  if (msg.timestamp) el.dataset.messageTs = msg.timestamp;
  if (msg.role === 'separator') {
    el.className = 'separator-line group/sep';
    if (msg.thinking_seconds != null) el.dataset.thinkingSeconds = String(msg.thinking_seconds);
    el.__baseHeight = 30;
    el.appendChild(new FakeElement('DIV', {className: 'flex-1 border-t'}));
    if (msg.event_index != null) {
      el.appendChild(new FakeElement('BUTTON', {className: 'recap-toggle p-0.5 text-slate-500'}));
    }
    return el;
  }
  const bubble = new FakeElement('DIV', {
    className: PROSE_ROLES.includes(msg.role) ? 'prose-msg' : 'whitespace-pre-wrap',
  });
  if (PROSE_ROLES.includes(msg.role)) bubble.dataset.raw = msg.content;
  else bubble.textContent = msg.content;
  const inner = new FakeElement('DIV', {className: 'max-w-[90%]'});
  inner.appendChild(bubble);
  el.appendChild(inner);
  el.__baseHeight = 46;
  return el;
}

function makeEngineTimers() {
  return {now: 0, idle: [], raf: [], timeout: []};
}

function installScrollTopClamp(root) {
  let scrollTop = root.scrollTop;
  Object.defineProperty(root, 'scrollTop', {
    configurable: true,
    get() {
      return scrollTop;
    },
    set(value) {
      scrollTop = Math.max(0, Math.min(value, root.scrollHeight - root.clientHeight));
    },
  });
}

function mountEngine(messages, {clientHeight = 900, clampScrollTop = false} = {}) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = clientHeight;
  if (clampScrollTop) installScrollTopClamp(root);
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  const control = new FakeElement('DIV', {id: 'page-depth-control'});
  for (const depth of DEPTHS) {
    const btn = new FakeElement('BUTTON', {className: 'turn-depth-btn'});
    btn.dataset.pageDepth = depth;
    control.appendChild(btn);
  }
  const timers = makeEngineTimers();
  const {context} = loadChatContext({
    createTreeWalker() {
      return {};
    },
    getElementById(id) {
      if (id === 'messages') return root;
      if (id === 'streaming-msg') return stream;
      if (id === 'page-depth-control') return control;
      return null;
    },
  });
  context.performance = {now: () => timers.now};
  context.requestIdleCallback = (fn) => (timers.idle.push(fn), timers.idle.length);
  context.requestAnimationFrame = (fn) => (timers.raf.push(fn), timers.raf.length);
  context.setTimeout = (fn) => (timers.timeout.push(fn), timers.timeout.length);
  context.clearTimeout = () => {};
  context.fetch = async () => ({ok: false, status: 500, json: async () => ({})});
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  const engine = context.Chat.TurnEngine.mountIfAvailable(root, messages, 'sess-eng');
  assert.ok(engine, 'engine mounts on a document with createTreeWalker');
  return {context, root, stream, control, timers, engine};
}

// --- deterministic scheduling ------------------------------------------------
function flushRaf(timers) {
  timers.raf.splice(0).forEach((fn) => fn());
}

// Let every queued slice run until the queues settle; the clock keeps jumping
// past the scroll-quiet window so gated slices become eligible.
function settle(timers) {
  for (let guard = 0; guard < 1000; guard++) {
    timers.now += 500;
    const idle = timers.idle.splice(0);
    const timeouts = timers.timeout.splice(0);
    const raf = timers.raf.splice(0);
    if (!idle.length && !timeouts.length && !raf.length) return;
    idle.forEach((fn) => fn());
    timeouts.forEach((fn) => fn());
    raf.forEach((fn) => fn());
  }
  throw new Error('engine scheduling did not settle');
}

function scrollTo(timers, root, position) {
  root.scrollTop = position;
  root.fire('scroll');
  timers.now += 16;
  flushRaf(timers);
}

function distanceFromBottom(root) {
  return root.scrollHeight - root.scrollTop - root.clientHeight;
}

// --- engine invariant readers --------------------------------------------------
function containerDescendants(root) {
  let count = 0;
  const walk = (node) => {
    for (const child of node.childNodes) {
      count++;
      if (child.nodeType === ELEMENT_NODE) walk(child);
    }
  };
  walk(root);
  return count;
}

function styleHeightPx(el) {
  const height = el.style.height;
  assert.ok(typeof height === 'string' && height.endsWith('px'), `spacer lacks a px height: ${height}`);
  return parseFloat(height);
}

function engineDebug(context, root) {
  const debug = context.Chat.TurnEngine.debug(root);
  assert.ok(debug, 'engine debug is available');
  return debug;
}

// 4.1 outline: DOM skeleton order + spacer arithmetic, exact within the
// engine's own height model.
function assertEngineInvariants(context, root, stream, label) {
  const kids = root.children;
  const topI = kids.findIndex((el) => el.classList.contains('turn-spacer-top'));
  const bottomI = kids.findIndex((el) => el.classList.contains('turn-spacer-bottom'));
  assert.ok(topI !== -1 && bottomI !== -1 && topI < bottomI, `${label}: spacer skeleton order`);
  assert.equal(kids[kids.length - 1], stream, `${label}: streaming fixture stays last`);

  const debug = engineDebug(context, root);
  const {start, end} = debug.window;
  const topExpected = start < debug.offsets.length ? debug.offsets[start] : 0;
  const windowEnd = end >= start ? debug.offsets[end] + debug.heights[end] : topExpected;
  const bottomExpected = debug.totalHeight - windowEnd;
  assert.equal(styleHeightPx(kids[topI]), Math.round(topExpected), `${label}: top spacer`);
  assert.equal(styleHeightPx(kids[bottomI]), Math.round(Math.max(0, bottomExpected)), `${label}: bottom spacer`);
  assert.ok(
      Math.abs((topExpected + (windowEnd - topExpected) + bottomExpected) - debug.totalHeight) < 1e-9,
      `${label}: spacer sum + window content = full height`);

  // The window in the DOM is exactly one contiguous run of segments.
  const windowNodes = kids.slice(topI + 1, bottomI);
  assert.equal(windowNodes.length, Math.max(0, end - start + 1), `${label}: window segment count`);
  return debug;
}

function assertFoldedWrapsBodyFree(root, label) {
  root.querySelectorAll('.turn-wrap').forEach((wrap) => {
    if (wrap.dataset.turnOpen !== 'false') return;
    const bodies = [];
    const walk = (node) => {
      for (const child of node.children) {
        if (child.dataset.messageId) bodies.push(child.dataset.messageId);
        walk(child);
      }
    };
    walk(wrap);
    assert.deepEqual(bodies, [], `${label}: folded wrap ${wrap.dataset.turnKey} holds body nodes`);
  });
}

function wrapsByKey(root) {
  const found = new Map();
  root.querySelectorAll('.turn-wrap').forEach((wrap) => found.set(wrap.dataset.turnKey, wrap));
  return found;
}

function messageIdsUnder(el) {
  const ids = [];
  const walk = (node) => {
    for (const child of node.children) {
      if (child.dataset.messageId) ids.push(child.dataset.messageId);
      walk(child);
    }
  };
  walk(el);
  return ids;
}

// The fake DOM cannot parse innerHTML, so the real recap flow (which builds
// its panel body from an HTML string) is stubbed at the Chat seam. The stub
// keeps the open/close state machine and the engine's registry feedback.
function stubRecapToggle(context, root) {
  const stub = function (btn) {
    const sep = btn.closest('.separator-line');
    const next = sep.nextElementSibling;
    if (next && next.classList.contains('recap-panel')) {
      next.remove();
    } else {
      sep.parentNode.insertBefore(new FakeElement('DIV', {className: 'recap-panel'}), sep.nextSibling);
    }
    const engine = context.Chat.TurnEngine.activeFor(root);
    if (engine) engine.noteRecapToggle(btn);
  };
  context.Chat.toggleRecapPanel = stub;
  context.toggleRecapPanel = stub;
}

// --- (a)+(b): bounded DOM and spacer arithmetic through full-history paging ---
test('turn engine: DOM stays bounded and spacer arithmetic holds while paging the full history', () => {
  const PAGES = 12;
  const perPage = 4;
  const pages = [];
  for (let p = 0; p < PAGES; p++) pages.push(ePage(`pg${p}_`, perPage, 2));

  const {context, root, stream, timers, engine} = mountEngine([...pages[PAGES - 2], ...pages[PAGES - 1]]);
  settle(timers);

  // DOM node cap, calibrated 2026-08-08 in this fake layout by running this
  // exact scenario with CALIBRATE_DOM_CAP=1: 144 nodes after mount, max 296
  // across the full 48-turn history page-through (the ±2-screen margins size
  // the window at folded-row heights). Cap set at 400 — history length never
  // enters the bound (plan 4.1③).
  // Browser-side absolute bound (outline, 1440x900 viewport, real Chrome
  // 2026-08-08, worktree instance): domNodeCount after paging all 1,055
  // messages of the heavy session was 4,020 at the top, 4,011 at the bottom,
  // versus 3,427 for the transient first-landing window. Calibrated
  // document-level cap for outline at this viewport: 4,400.
  const DOM_NODE_CAP = 400;

  const debug0 = assertEngineInvariants(context, root, stream, 'after mount');
  assertFoldedWrapsBodyFree(root, 'after mount');
  const nodesAfterMount = containerDescendants(root);
  assert.ok(nodesAfterMount <= DOM_NODE_CAP, `after mount: ${nodesAfterMount} nodes`);
  assert.equal(debug0.entries, 2 * 4 * 5, 'two pages in the store');

  let maxNodes = nodesAfterMount;
  for (let p = PAGES - 3; p >= 0; p--) {
    const shift = engine.prependMessages(pages[p]);
    assert.ok(shift > 0, `page ${p}: content shifted down`);
    settle(timers);
    if (p % 2 === 0) {
      // Interleave scroll positions across the grown history.
      const debug = engineDebug(context, root);
      scrollTo(timers, root, Math.max(0, Math.floor(debug.totalHeight / 2)));
      settle(timers);
      scrollTo(timers, root, Math.max(0, debug.totalHeight - root.clientHeight));
      settle(timers);
    }
    const label = `after page ${p}`;
    const debug = assertEngineInvariants(context, root, stream, label);
    assertFoldedWrapsBodyFree(root, label);
    assert.equal(debug.entries, (PAGES - p) * perPage * 5, `${label}: store holds every paged message`);
    const nodes = containerDescendants(root);
    maxNodes = Math.max(maxNodes, nodes);
    assert.ok(nodes <= DOM_NODE_CAP, `${label}: ${nodes} nodes (cap ${DOM_NODE_CAP})`);
    // Window membership stays capped regardless of history length.
    assert.ok(debug.window.end - debug.window.start + 1 <= context.Chat.TurnEngine.MAX_WINDOW_TURNS,
        `${label}: window turn count ${debug.window.end - debug.window.start + 1}`);
  }

  // The whole-history end state carries no more DOM than the two-page start.
  if (process.env.CALIBRATE_DOM_CAP) console.log('CALIBRATION maxNodes', maxNodes, 'afterMount', nodesAfterMount);
  assert.ok(maxNodes <= DOM_NODE_CAP, `max nodes ${maxNodes} <= ${DOM_NODE_CAP}`);
  const finalDebug = engineDebug(context, root);
  assert.equal(finalDebug.entries, PAGES * perPage * 5, 'full history in the store');
  assert.equal(finalDebug.entries / 5, 48, '48 finished turns derived');
});

// --- scroll gating: no idle slices during scroll activity, placeholders shown ---
test('turn engine: pre-render pauses during scroll activity and unready turns use placeholders', () => {
  const {context, root, timers, engine} = mountEngine([...ePage('sg0_', 4), ...ePage('sg1_', 4)]);
  const stream = root.children[root.children.length - 1];

  // Right after mount (no idle slice has run): the open newest turn is
  // unready and shows as an estimated-height placeholder row; folded rows
  // need no bodies and materialize immediately.
  const debugAtMount = engineDebug(context, root);
  assert.ok(root.querySelectorAll('.turn-placeholder').length >= 1, 'placeholder shown for the unready open turn');
  assert.equal(debugAtMount.stats.prerenderAtoms.length, 0, 'no atom ran before idle');
  assertEngineInvariants(context, root, stream, 'at mount');

  // Scroll fires: the trailing activity window must freeze the queue.
  root.scrollTop = 10;
  root.fire('scroll');
  timers.now += 16;
  flushRaf(timers);
  const idleCallbacks = timers.idle.splice(0);
  assert.ok(idleCallbacks.length >= 1, 'an idle slice is armed');
  idleCallbacks.forEach((fn) => fn());   // fires inside the activity window
  assert.equal(engine.stats.prerenderAtoms.length, 0, 'no pre-render atom started mid-scroll');
  assert.ok(engine.stats.slicesDeferredForScroll >= 1, 'the slice deferred itself');
  assert.equal(engine.stats.slicesStartedDuringScrollWindow, 0, 'no slice ever started inside the window');

  // Once quiet, the queue drains and placeholders are swapped by reprojection.
  settle(timers);
  assert.equal(root.querySelectorAll('.turn-placeholder').length, 0, 'placeholders replaced once ready');
  assertEngineInvariants(context, root, stream, 'after drain');
  assert.equal(engine.stats.slicesStartedDuringScrollWindow, 0, 'still zero after the full drain');
});

// --- plan 2 v4 wheel-pin legs ----------------------------------------------
test('turn engine: 100px wheel steps monotonically leave the bottom band', () => {
  const {root, timers} = mountEngine(ePage('wheel_', 24), {
    clientHeight: 120,
    clampScrollTop: true,
  });
  settle(timers);

  root.scrollTop = root.scrollHeight;
  assert.equal(distanceFromBottom(root), 0, 'fixture starts at the real bottom');

  let previous = root.scrollTop;
  for (let i = 0; i < 12; i++) {
    scrollTo(timers, root, previous - 100);
    assert.ok(root.scrollTop < previous, `wheel step ${i} moved upward without write-back`);
    previous = root.scrollTop;
  }
  assert.ok(distanceFromBottom(root) > 150, 'small wheel steps escaped the follow band');
});

test('turn engine: quiet prerender-ready reproject preserves a one-notch landing', () => {
  const {root, timers, engine} = mountEngine(ePage('ready_', 24), {
    clientHeight: 120,
    clampScrollTop: true,
  });
  settle(timers);

  root.scrollTop = root.scrollHeight - 100;
  const landedScrollTop = root.scrollTop;
  assert.ok(distanceFromBottom(root) < 150, 'fixture is one notch inside the follow band');

  timers.now += 250;
  engine.reproject('prerender-ready');
  assert.equal(root.scrollTop, landedScrollTop, 'quiet pre-render does not pin the viewport');
});

test('turn engine: append follow matches the legacy 150px auto-scroll band', () => {
  function appendAtDistance(distance) {
    const state = mountEngine(ePage(`append_${distance}_`, 24), {
      clientHeight: 120,
      clampScrollTop: true,
    });
    settle(state.timers);
    state.root.scrollTop = state.root.scrollHeight - state.root.clientHeight - distance;
    const before = state.root.scrollTop;
    assert.equal(distanceFromBottom(state.root), distance, `fixture landed ${distance}px from bottom`);
    state.engine.appendMessage(eMsg('assistant', `append-${distance}`, 'new answer'), false);
    return {state, before};
  }

  const exact = appendAtDistance(0);
  assert.equal(distanceFromBottom(exact.state.root), 0, 'append at the exact bottom follows');

  const insideBand = appendAtDistance(149);
  assert.equal(distanceFromBottom(insideBand.state.root), 0, 'append inside 150px follows');

  const outsideBand = appendAtDistance(150);
  assert.equal(outsideBand.state.root.scrollTop, outsideBand.before,
      'append at exactly 150px does not follow');
  assert.ok(distanceFromBottom(outsideBand.state.root) > 150,
      'append outside the legacy band leaves the viewport where the user put it');
});

test('turn engine: a resize/clamp landing at the bottom still follows a later append', () => {
  const {root, timers, engine} = mountEngine(ePage('resize_', 24), {
    clientHeight: 300,
    clampScrollTop: true,
  });
  settle(timers);

  root.clientHeight = 480;
  root.scrollTop = root.scrollHeight;
  assert.equal(distanceFromBottom(root), 0, 'clamped resize landing is exactly at bottom');

  engine.appendMessage(eMsg('assistant', 'resize-append', 'follow the resized viewport'), false);
  assert.equal(distanceFromBottom(root), 0, 'positional append follow needs no resize observer');
});

test('turn engine: an engine snap echo does not defer the pre-render queue', () => {
  const {root, timers, engine} = mountEngine(ePage('echo_', 12), {
    clientHeight: 900,
    clampScrollTop: true,
  });
  const initialIdle = timers.idle.splice(0);
  assert.equal(initialIdle.length, 1, 'mount arms one pre-render slice');

  root.scrollTop = root.scrollHeight;
  engine.appendMessage(eMsg('assistant', 'echo-append', 'echo marker'), false);
  assert.equal(distanceFromBottom(root), 0, 'append produced an engine snap');

  const deferredBeforeEcho = engine.stats.slicesDeferredForScroll;
  root.fire('scroll');
  assert.equal(timers.raf.length, 0, 'the engine echo does not schedule a scroll frame');
  assert.equal(engine.stats.slicesDeferredForScroll, deferredBeforeEcho,
      'the engine echo is not counted as user activity');

  initialIdle.forEach((fn) => fn());
  assert.equal(engine.stats.slicesDeferredForScroll, deferredBeforeEcho,
      'the queued slice runs without a false quiet-window deferral');
});

test('turn engine: the 900px wheel path still leaves the bottom follow band', () => {
  const {root, timers} = mountEngine(ePage('large_', 24), {
    clientHeight: 120,
    clampScrollTop: true,
  });
  settle(timers);

  root.scrollTop = root.scrollHeight;
  const before = root.scrollTop;
  scrollTo(timers, root, before - 900);
  assert.ok(root.scrollTop < before, 'large wheel step moves upward');
  assert.ok(distanceFromBottom(root) > 150, 'large wheel step remains outside the follow band');
});

// --- (c): open/override, expanded N steps and open recap survive eviction ----
test('turn engine: override, expanded steps bar and open recap survive window eviction and re-materialization', () => {
  const msgs = [];
  for (let i = 0; i < 12; i++) msgs.push(...eTurn('st_', i, 2));
  const {context, root, timers} = mountEngine(msgs, {clientHeight: 120});
  stubRecapToggle(context, root);
  settle(timers);

  const keyX = eTurnKey('st_', 5);
  const debug = () => engineDebug(context, root);
  const indexOfX = () => debug().keys.indexOf(keyX);

  // Bring X into the window, then set its state from the user surface.
  scrollTo(timers, root, debug().offsets[indexOfX()] + 1);
  settle(timers);
  let wrap = wrapsByKey(root).get(keyX);
  assert.ok(wrap, 'X materialized at its own scroll position');

  wrap.querySelector('.turn-row').onclick.call(wrap.querySelector('.turn-row'));
  settle(timers);
  wrap = wrapsByKey(root).get(keyX);
  assert.equal(wrap.dataset.turnOpen, 'true', 'X opened from its row');
  assert.ok(wrap.querySelectorAll('.turn-fold-bar').length > 0 || debug().heights[indexOfX()] > 0);

  const bar = wrap.querySelector('.turn-fold-bar');
  context.toggleTurnFold(bar);
  settle(timers);
  wrap = wrapsByKey(root).get(keyX);
  assert.equal(wrap.querySelector('.turn-fold-content').classList.contains('hidden'), false,
      'steps band expanded');

  const sep = wrap.querySelector('.separator-line');
  context.toggleRecapPanel(sep.querySelector('.recap-toggle'), 'sess-eng', 1005);
  assert.ok(sep.nextElementSibling && sep.nextElementSibling.classList.contains('recap-panel'),
      'recap panel opened next to the separator');

  // Evict X from the window entirely.
  const lastIndex = debug().segments - 1;
  scrollTo(timers, root, debug().offsets[lastIndex]);
  settle(timers);
  scrollTo(timers, root, debug().totalHeight - root.clientHeight);
  settle(timers);
  assert.equal(wrapsByKey(root).has(keyX), false, 'X left the DOM after eviction');

  // Re-enter: every piece of state comes back from the registry.
  scrollTo(timers, root, debug().offsets[indexOfX()] + 1);
  settle(timers);
  wrap = wrapsByKey(root).get(keyX);
  assert.ok(wrap, 'X re-materialized');
  assert.equal(wrap.dataset.turnOpen, 'true', 'manual open survived eviction');
  assert.ok(messageIdsUnder(wrap).length > 0, 'body rebuilt');
  assert.equal(wrap.querySelectorAll('.turn-collapse').length, 1, 'collapse control is not duplicated');
  assert.equal(wrap.querySelector('.turn-fold-content').classList.contains('hidden'), false,
      'expanded steps band survived eviction');
  const sep2 = wrap.querySelector('.separator-line');
  assert.ok(sep2.nextElementSibling && sep2.nextElementSibling.classList.contains('recap-panel'),
      'open recap panel restored');
});

// --- (d): prepending pages never re-derives or re-materializes settled wraps ---
test('turn engine: prepending pages leaves existing window wrappers untouched', () => {
  const pages = [];
  for (let p = 0; p < 6; p++) pages.push(ePage(`kp${p}_`, 4, 2));
  const {context, root, timers, engine} = mountEngine([...pages[4], ...pages[5]]);
  settle(timers);

  const before = wrapsByKey(root);
  assert.ok(before.size >= 1, 'window holds wraps');
  const materializationsBefore = engine.stats.segmentMaterializations;

  engine.prependMessages(pages[3]);
  settle(timers);
  engine.prependMessages(pages[2]);
  settle(timers);

  assert.equal(engine.stats.rederivesOfSettledTurns, 0, 'turn-aligned pages consumed no settled segment');
  assert.equal(engine.stats.segmentMaterializations, materializationsBefore,
      'no in-window wrap was rebuilt by pagination');
  const after = wrapsByKey(root);
  for (const [key, el] of before) {
    assert.equal(after.get(key), el, `wrap ${key} is the same DOM node after two prepends`);
  }
});

test('turn engine: archived pages with a settled prefix preserve their partial boundary span', () => {
  const initial = [
    eMsg('assistant', 'tail-a', 'continuation'),
    eMsg('assistant', 'tail-c', 'conclusion'),
    eMsg('separator', 'tail-p', '', {thinking_seconds: 12, event_index: 3000}),
  ];
  const {context, root, timers, engine} = mountEngine(initial, {clientHeight: 900});
  settle(timers);

  engine.prependMessages([
    ...ePage('archived_', 1, 1),
    eMsg('user', 'boundary-h', 'the earlier ask'),
    eMsg('assistant', 'boundary-s', 'the earlier continuation'),
  ]);
  settle(timers);

  const debug = engineDebug(context, root);
  assert.deepEqual([...debug.keys], [
    'archived_t0_h0|archived_t0_c0|archived_t0_p0',
    'boundary-h|tail-c|tail-p',
  ]);
  assert.equal(debug.entries, 9, 'all page and boundary messages stay in the store');
  assert.equal(debug.pending.some(Boolean), false, 'the boundary turn is settled');
  assertEngineInvariants(context, root, root.children[root.children.length - 1], 'archived mixed page');
});

test('turn engine: live messages without server ids receive stable turn identities', () => {
  const {context, root, timers, engine} = mountEngine([]);
  engine.appendMessage({role: 'user', content: 'the live ask'}, true);
  engine.appendMessage({role: 'assistant', id: 'live-answer', content: 'the live answer'}, true);
  engine.appendMessage({role: 'separator', id: 'live-separator', thinking_seconds: 4}, false);
  settle(timers);

  const debug = engineDebug(context, root);
  assert.match(debug.keys[0], /^client:sess-eng:1\|live-answer\|live-separator$/);
  const row = root.querySelector('.turn-row');
  assert.equal(row.querySelector('.turn-row-tag').textContent, 'You');
  assert.ok(messageIdsUnder(root).includes('client:sess-eng:1'), 'the optimistic user is rendered in the turn');
});

// --- (e): non-aligned archived page heads and the live tail never block -------
test('turn engine: archived non-aligned page heads and an unfinished live tail stay flat and unblocked', () => {
  // Archived middle: a span that starts mid-turn (no stimulus) but still
  // separator-terminated, then an unfinished live tail.
  const initial = [
    eMsg('assistant', 'arc-a1', 'mid-turn continuation'),
    eMsg('assistant', 'arc-a2', 'mid-turn conclusion'),
    eMsg('separator', 'arc-s1', '', {thinking_seconds: 21, event_index: 2000}),
    eMsg('user', 'live-head', 'the live ask'),
    eMsg('assistant', 'live-body', 'partial live answer'),
  ];
  const {context, root, stream, timers, engine} = mountEngine(initial);
  settle(timers);

  let debug = engineDebug(context, root);
  assert.equal(debug.pending.filter(Boolean).length, 1, 'live tail is a pending flat segment');
  assertEngineInvariants(context, root, stream, 'archived mount');

  // An older page whose bottom is mid-turn merges into the leading span and
  // closes it as one turn (head = the page's own user message).
  const older = [
    eMsg('user', 'old-h', 'the original ask'),
    eMsg('assistant', 'old-s1', 'a step from before'),
  ];
  engine.prependMessages(older);
  settle(timers);

  debug = engineDebug(context, root);
  assert.equal(debug.keys[0], 'old-h|arc-a2|arc-s1', 'boundary span re-derived across the page seam');
  assert.equal(debug.pending[debug.pending.length - 1], true, 'live tail still pending');
  assert.equal(debug.queueLength, 0, 'pre-render fully drained around the non-aligned page');
  assertEngineInvariants(context, root, stream, 'after non-aligned prepend');

  // The live stream finishes: the separator lands, the new turn opens and the
  // previously open turn folds, all inside the engine.
  engine.appendMessage({role: 'separator', id: 'live-sep', thinking_seconds: 9, event_index: 2001}, false);
  settle(timers);

  debug = engineDebug(context, root);
  assert.equal(debug.pending.every((p) => !p), true, 'no pending span after the live separator');
  assert.equal(debug.keys[debug.keys.length - 1], 'live-head|live-body|live-sep', 'live tail became a turn');
  const wraps = wrapsByKey(root);
  const oldWrap = wraps.get('old-h|arc-a2|arc-s1');
  const liveWrap = wraps.get('live-head|live-body|live-sep');
  assert.equal(oldWrap.dataset.turnOpen, 'false', 'previously open turn folded on landing');
  assert.equal(liveWrap.dataset.turnOpen, 'true', 'newest finished turn is open');
  assertEngineInvariants(context, root, stream, 'after live separator');
});
