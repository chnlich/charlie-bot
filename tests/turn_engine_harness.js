// ---------------------------------------------------------------------------
// Shared harness for the chat turn-layout suites: the fake DOM the vm loader
// runs the chat bundle in (FakeElement with deterministic layout), the chat
// context loader, the DOM-path mount rig, and the turn-engine drive train
// (queueable timers, engine mount, invariant readers). The suites keep their
// own expectations; everything here only builds inputs and reads the DOM back.
// ---------------------------------------------------------------------------

const assert = require('node:assert/strict');
const vm = require('node:vm');

const {chatModules, runStaticModules} = require('./read_static');
const { createClassList } = require('./dom_element_stub');
const { escapeHtmlText } = require('./escape_html_stub');

const ELEMENT_NODE = 1;
const TEXT_NODE = 3;

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
    this.classList = createClassList(className);
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
    this.classList = createClassList(this._className);
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
    this.innerHTML = escapeHtmlText(value);
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
  runStaticModules(context, chatModules());
  return {context, nowIso};
}

const PROSE_ROLES = ['assistant', 'worker_summary', 'plan'];
const DEPTHS = ['outline', 'compact', 'expanded'];
const BUBBLE_TIME_TEXT = 'Apr 2, 2026, 3:04:05 AM PDT';

// The #page-depth-control rig renderSessionView's depth reproject resolves:
// one button per DEPTHS entry, carrying its depth in dataset.pageDepth.
function buildDepthControl() {
  const control = new FakeElement('DIV', {id: 'page-depth-control'});
  for (const depth of DEPTHS) {
    const btn = new FakeElement('BUTTON', {className: 'turn-depth-btn'});
    btn.dataset.pageDepth = depth;
    control.appendChild(btn);
  }
  return control;
}

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
  const control = buildDepthControl();
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

function wrappers(root) {
  return root.children.filter((el) => el.classList.contains('turn-wrap'));
}

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

// The timing seams the turn engine drives; the recorders stay observable so a
// test can flush each queue deterministically.
function installEngineTimers(context, timers) {
  context.performance = {now: () => timers.now};
  context.requestIdleCallback = (fn) => (timers.idle.push(fn), timers.idle.length);
  context.requestAnimationFrame = (fn) => (timers.raf.push(fn), timers.raf.length);
  context.setTimeout = (fn) => (timers.timeout.push(fn), timers.timeout.length);
  context.clearTimeout = () => {};
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
  const control = buildDepthControl();
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
  installEngineTimers(context, timers);
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

module.exports = {
  ELEMENT_NODE,
  TEXT_NODE,
  FakeText,
  FakeElement,
  loadChatContext,
  PROSE_ROLES,
  DEPTHS,
  BUBBLE_TIME_TEXT,
  buildDepthControl,
  msg,
  separator,
  plain,
  fixture,
  buildElement,
  mountCase,
  wrappers,
  eMsg,
  eTurn,
  ePage,
  eTurnKey,
  fakeEngineNode,
  makeEngineTimers,
  installEngineTimers,
  installScrollTopClamp,
  mountEngine,
  flushRaf,
  settle,
  scrollTo,
  distanceFromBottom,
  containerDescendants,
  styleHeightPx,
  engineDebug,
  assertEngineInvariants,
  assertFoldedWrapsBodyFree,
  wrapsByKey,
  messageIdsUnder,
  stubRecapToggle,
};
